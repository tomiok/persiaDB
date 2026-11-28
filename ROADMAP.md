# Persia DB — Roadmap

**Versions.** M0–M15 are the MVP and ship as v1.0. Milestones M16 and later are post-MVP; each heading names its target version. The percentage and burn-up in [`PROGRESS.md`](./PROGRESS.md) count M0–M15 only. A leaf of M0–M15 labelled with a later version (`(v1.1)`, `(post-v1)`: right after its id, or ending its text, before any "Done as:" remark; `cargo xtask progress` rejects one placed elsewhere) is post-MVP work filed under its milestone: it counts with the post-MVP leaves and does not hold its milestone, or the ones depending on it, open.

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

## 0.6 Status honesty and hardening (found in the 2026-11-02 audit)
- [x] 0.6.1 `cargo xtask progress` rewrites marked progress blocks in `README.md` (headline count, milestone table), `--check` fails when they are stale, and `repo_checks` tests fail when the README blocks differ from what ROADMAP.md and the history render, or when the README quickstart is not the one `readme_quickstart.rs` runs
- [x] 0.6.2 README claims only what the code does: the engine's write path and search are described as they are, and the relaxation tagline is design intent (M8), not a feature
- [x] 0.6.3 Website status from `website/src/status.mjs`: `features.mdx` and `guide/getting-started.md` take their status sentences from it, and it sees the collection search (`CollectionViews::search`, `db/search.rs`)
- [x] 0.6.4 Placeholder crates say so: `persia`, `persia-blob`, `persia-proto` docs read "Placeholder for M9/M10/M11", and `persia-server` prints "not implemented (M11)" and exits non-zero
- [x] 0.6.5 No unchecked `as` in `persia-format`, `persia-storage` and `persia-engine`: the pedantic cast lints were already on, so deny `clippy::as_conversions` outside tests; each `as` becomes `From`, a `widen` helper, `try_from`, or carries a justified `#[allow]` (CLAUDE.md "checked ops")
- [x] 0.6.6 Miri on the pure parsers of `persia-engine` (CI job and `just miri`), beside `persia-format`: documents, tombstones, section kinds, norms, the FST check, the manifest, WAL and document codecs; property tests, fixture readers and sweeps over 90 s are left to native runs
- [x] 0.6.7 A container that cannot map its file falls back to positioned reads with a `tracing::warn!`, and says whether it is mapped (`is_mapped()`) and how many remaps failed (`mapping_failures()`)
- [x] 0.6.8 `enable_mapped_reads` becomes an `unsafe fn` whose `# Safety` section states the caller's contract (no other process truncates or rewrites the mapped range, ADR-0010, ADR-0017)
- [x] 0.6.9 Guard `just miri`'s filters: a repository check that each test filter of the recipe selects at least one test and each `--skip` matches only the tests it names, so a rename cannot silently drop Miri coverage (found in the 0.6.6 review)
- [ ] 0.6.10 A test a `just miri` filter picks up does not slow the job unnoticed (found in the 0.6.9 review): the `miri` CI job reports the duration of each `cargo miri test` line and fails when one exceeds its budget, naming the slowest tests, so a new slow test under `document::` or in a codec test file is skipped or shrunk in its own PR, not found by a timeout
- [x] 0.6.11 (v1.0) No async runtime in the embedded stack (ADR-0064): `cargo xtask check-deps` walks the resolved dependency graph (`cargo metadata`'s `resolve`: normal, build and dev edges, every platform) of `persia`, `persia-engine`, `persia-blob`, `persia-storage`, `persia-format`, `persia-analysis` and `persia-cli` and fails if it reaches `tokio`, `async-std`, `smol` or an executor or reactor they are built from (`ASYNC_RUNTIMES` in `xtask/src/check_deps/no_async.rs`), naming the path. `persia-server` may depend on one and uses the embedded crates through their synchronous API; `ALLOWED` is unchanged. [T] On a fake graph a direct, a transitive, a build and a dev-only runtime are each rejected with their path, and `persia-server` depending on tokio is accepted Done as: `xtask/src/check_deps/no_async.rs`, run by `cargo xtask check-deps` over `cargo metadata --all-features` (an embedded crate missing from the graph is an error); fake-graph tests plus real throwaway workspaces for a feature-gated and a renamed runtime; CLAUDE.md states the rule (ADR-0064).

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
- [x] 2.3.11 A listing or sweep of temporary files unlocks a file it keeps before closing it (ADR-0019 decision 7; found by a one-off failure of `a_temporary_a_live_process_holds_is_kept_until_the_process_is_gone`): a child that another thread forked kept the lock past the close until its `exec`, and the next sweep kept a stale file as in use; [T] `ForkedFs` keeps a duplicate of every descriptor (`tests/sweep_inherited_locks.rs`)
- [x] 2.3.12 A `put` or `put_if_absent` that fails leaving its temporary file (the cleanup removal failed) unlocks it before closing it (ADR-0019 decision 7; found in the 2.3.11 review): a child forked meanwhile keeps the lock until its `exec`, and a sweep then reports the file in use once. No data at risk

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
- [x] 6.1.6 Undo a failed `put` without rehashing the term arenas: `Arena::truncate` rebuilds the whole table under the write lock, O(terms) per rejected document (found in the 6.1 review) Done as: `Arena::truncate` empties the slots of the forgotten ids, highest first, instead of rehashing the table: the table always equals inserting the ids in order, so no lower id's probe passes a higher one's slot. Property test `truncate_matches_a_full_rehash` (slot for slot against the old rehash, across growth). Bench `memtable/put_rejected`: 18.5 K → 20.6 K rejected puts/s at 2 000 documents (docs/benchmarks.md).
- [ ] 6.1.7 Cache `MemtableView::df` if term statistics become hot: it walks the posting chain under the read lock on every call. The count is the view's `(docs, version)` cutoff, so it cannot reuse a live counter on the term, and a cache has to be keyed by that cutoff and bounded (found in 7.2) (v1.1)

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
- [ ] 6.3.11 Flush without re-indexing: build the segment from the memtable's postings instead of analyzing every document again (ADR-0029, found in 6.3.1); bench the flush before and after (v1.1)
- [x] 6.3.12 `put_batch` makes a document visible only once it is durable (today the ones before the last are visible once buffered, SPEC §8.2.1); test with a forced sync failure that no reader sees an unsynced document (found in the 2026-11-02 audit)

## 6.4 Upserts & deletes across segments
- [x] 6.4.1 Lookup docid by external id newest→oldest segment (IdMap FST)
- [x] 6.4.2 Tombstone sidecar format + versioning (SPEC §8.3, ADR); the writer builds each version as a superset of the previous one, checked before commit (the manifest only sees counts, SPEC §8.7.2)
- [x] 6.4.3 Delete-by-id and upsert through commit; visibility rules (read-your-writes). Until then a flush keeps only the memtable's live documents, so a delete or upsert of an id in an older segment is lost at the flush (ADR-0029, found in 6.3)
- [x] 6.4.4 [T] differential tests vs. reference model for random put/delete/commit sequences
- [ ] 6.4.5 Prune the commit's tombstone lookups by each segment's footer id range (`Segment::id_range`), once open or `Segment::verify` checks `min_id`/`max_id` against the `IdMap` (a wrong range would leave an older copy live). Helps time-ordered ids only; random ids (UUIDs) span every range. Bench `commit_inserts_*` before and after (found in 6.4.3: 64 segments double a 2 000-document commit) (v1.1)
- [x] 6.4.6 Drop a segment from the manifest in the commit that deletes its last live document, instead of keeping it listed with `deleted = doc_count` until a merge (ADR-0030 item 8)
- [ ] 6.4.7 Per-segment id membership filter (e.g. a Bloom filter section) so a commit skips segments that cannot hold an id, for random ids that id ranges do not prune; a format change (SPEC §5.3, ADR, golden file) (found in the 6.4 review) (v1.1)

## 6.5 Compaction / merging (SPEC §8.5)
- [x] 6.5.1 Merge policy (tiered) as pure function: segments → merge plans; [T] table tests
- 6.5.2 Segment merger: N readers → one writer, dropping tombstoned docs, remapping docids
  - [x] 6.5.2.1 Merge term dictionaries and postings (each input's terms into one dictionary the writer sorts, ADR-0031)
  - [x] 6.5.2.2 Merge columns, norms, doc store, id map, blob refs
  - [x] 6.5.2.3 Recompute statistics
- [x] 6.5.3 Background merge scheduler (thread pool, cancellation, backpressure); `Segment::verify` the merged segment before its commit, since a merge copies damage only `verify` finds (SPEC §5.5, found in the 6.5 review)
- [x] 6.5.4 Safe deletion of obsolete segments via snapshot epoch/refcount
- [x] 6.5.5 [T] property: merge(search results) == pre-merge results (same hits/scores within epsilon)
- [ ] 6.5.6 Streaming merge writer: encode the merged sections while the inputs are read, so a merge holds no more than a block per section instead of the whole merged segment in a `SegmentBuilder` (ADR-0031 item 8, found in 6.5.2) (v1.1)
- [ ] 6.5.7 Merge throughput: a merge-only postings reader that decodes whole lists (no per-term `Postings` cursor), and regrouping without a sort; goal: `segment_merge/copy_8` faster than `segment/add` on the same run (internal bench, `docs/benchmarks.md`; not a claim, BENCHMARKS.md) (v1.1)
- [x] 6.5.8 Engine-driven container compaction (SPEC §4.6): `Database` names the live frames (the manifest's, plus the obsolete frames still referenced, 6.5.4) and the WAL range, compacts when dead space reaches the ratio, and publishes the relocated manifest (`Manifest::relocate`) (found in 6.5.4)
- [x] 6.5.9 Merges that survive concurrent deletes: re-apply the deletions committed to its inputs since the plan to the merged segment (a docid map kept with the result) instead of discarding it. Test: the segment count stays bounded under steady deletes, replacing `deletes_into_every_segment_discard_every_merge_until_they_pause` (6.5.11), which pins today's unbounded growth (ADR-0032 item 4, found in 6.5.3)
- [x] 6.5.10 Until 6.5.8, say that a database file only grows (README, SPEC §4.6), and report its dead bytes (`Database::space_usage`) (found in the 2026-11-02 audit)
- [x] 6.5.11 Count the merges discarded because their inputs had deletes committed meanwhile (`Database::merge_stats`), and pin with a test what steady deletes do to the segment count: deletes into the inputs of every merge in flight keep any merge from committing, so the count grows until they pause (SPEC §8.7.2); 6.5.9 lifts that, and its test must then bound the count (found in the 2026-11-02 audit)
- [x] 6.5.12 Compaction in the randomized merge model: a SimFs (path) variant of `merge_interleavings_match_the_model` with a `Compact` op, so compactions interleave with threaded merges in flight, deletes, cancels and crash-reopens (found in the 6.5.8 review)
- [x] 6.5.13 A lookup by id finds the live copy wherever it is: after a merge caught up with an upsert (6.5.9), the merged segment, last, holds the old copy deleted, and the lookup stopped there and reported the document absent (SPEC §8.5; found by the kill-9 loop in CI, seed 104372101176064)
- [x] 6.5.14 Deterministic threaded interleavings in the merge model: a test-only gate that holds each finished merge until a `Release` op (or settles only on request), so `merge_interleavings_match_the_model` explores the same schedules on every machine; today whether a merge is in flight across an upsert depends on the worker's speed, and the model caught 6.5.13 only on a loaded machine (found in the 6.5.13 review)

## 6.6 Open/recovery orchestration
- [x] 6.6.1 Open sequence: superblock → manifest → replay WAL → memtable → ready (SPEC §8.6)
- [x] 6.6.2 Single-writer enforcement + read-only open for readers
- [x] 6.6.3 `persia verify` full database verification command
- [x] 6.6.4 Differential + crash-loop test harness (kill -9 child process in loop, reopen, check invariants) — shared with integration-tester
- [x] 6.6.5 Flush during replay: one collection's unflushed record holds `wal_start` back for all, so replay may hold more than a memtable threshold (or `MAX_DOCS` docids) of another collection; flush to a segment when a replayed memtable reaches its threshold instead of failing the open. Test: replay past the size threshold and the docid limit (found in the 6.2 review)
- [x] 6.6.6 Refresh a `ReadOnlyDatabase` in place: a new snapshot from the next manifest and the WAL frames written since, instead of a reopen (ADR-0033 Alternatives, found in 6.6.2) Done as: `ReadOnlyDatabase::refresh(&self) -> Result<Refreshed>` reopens the container (`Container::reopen_read_only`, at its path so it follows a compaction) and swaps the next state in through an `ArcSwap`: `Incremental` when the replayed WAL chunks are the first ones the new scan finds (frame for frame, and every one past the slot's `log_end`, which a power loss may still replace, with its payload's XXH3-128 unchanged) and no collection was dropped or changed, replaying only the chunks after them into the same memtables (held until the swap) and opening only changed segments; `Reloaded` (the open sequence, same registry and analysis) otherwise; `Unchanged` swaps nothing; another `db_uuid` or an older `seq` is `RefreshRefused`. `verify` and `blob_references` read one state throughout (SPEC §8.6, ADR-0060). Tests: `read_only_refresh`, `read_only_refresh_model` (proptest: refresh equals a fresh open, also across power losses), `read_only_refresh_crash` (SimFile fault, torn and power-loss images), `read_only_refresh_process` (kill -9 writer beside a refreshing reader), persia-storage `container` (`reopen_read_only_*`). Bench `read_only`: a one-put refresh 133 µs against a 7.93 ms reopen (docs/benchmarks.md).
- [x] 6.6.7 `persia verify` with custom analyzers, and distinct exit codes for corruption and other failures (found in the 6.6 review); custom analyzers moved to 6.6.8, since a schema names only built-in analyzers today (SPEC §6.2.2)
- [x] 6.6.8 `persia verify` with custom analyzers, once a schema can name an analyzer registered with `AnalyzerRegistry::register` (today `AnalyzerRef` accepts built-ins only, SPEC §6.2.2): a way to load them in the CLI, or a replay that validates records without analyzing their `text` fields (found in 6.6.7) Done as: a schema names a registered analyzer as a built-in one (`"name"`, `"name@version"`), with the same stored and canonical bytes (golden vector `canonical_encoding_golden_registered_analyzer`); `register` takes names of built-in shape only, so older builds report such a schema unsupported (property test over accepted names). A stored schema's non-built-in name decodes as registered; the open checks its registry (built-ins always added) for every collection before it reads any segment and fails with `DatabaseError::UnregisteredAnalyzer`, not corruption. `persia verify` opens with `ReadOnlyDatabase::open_skipping_analysis`: records of such fields are validated and applied without analysis, the `analysis` row names them, exit status 0 (SPEC §6.2.2, §6.3.3, §8.6; ADR-0059). No on-disk format change.
- [x] 6.6.9 A writer's open replays past a Manifest frame a cut commit left after `log_end`, then retires it with a commit that flushes nothing, so the WAL it logs next is not hidden behind it from later opens. A read-only open replays past that Manifest too while a fresh superblock read still leaves it after `log_end`, and stops only when the read names the frame as the committed manifest (SPEC §8.6, ADR-0033 item 7). Test: crash in a commit (and in the retiring open), reopen, write, crash, writer and read-only reopen; a synced put already behind the Manifest is visible to a read-only open before any writer retires it, and survives the retiring open with no later flush (found by the 6.6.4 crash loop in the 7.2 batch)
- [x] 6.6.10 `persia inspect container` (CLI, read-only): the superblock (`seq`, `wal_start`, `log_end`), every frame from `wal_start` (kind, offset, length, `frame_seq`, whether after `log_end`), Manifest frames after `log_end` with their `seq`, and with `--id` the WAL records naming a document id. Replaces the throwaway debug test the 6.6.9 triage needed (crash-triage skill)
- [x] 6.6.11 Dropping a writer releases the container lock before the descriptor is closed, so a duplicated or inherited descriptor (a `fork` before `exec`) does not keep the next `Database::open` out (SPEC §4.5.1, ADR-0033 item 6). Merge workers are joined first and do not hold the file. Test: a duplicate of the locked descriptor blocks a second open, and after drop the open succeeds while the duplicate is still open; a database open replays a backlog (flush), runs a background merge, drops, and opens again at once, including while a snapshot of the dropped database is held (found by the nightly kill-9 loop, `kill_9_loop_keeps_every_acknowledged_write`)
- [ ] 6.6.12 (v1.1) Detect a registered analyzer whose chain changed under the same version: a stored fingerprint of each registered analyzer version's output on a fixed probe text, checked at open (today a caller who changes a chain without a new version, or ignores a `RegisterError::Reserved` after a release adds that built-in name, opens the database with another chain unnoticed; ADR-0059 Consequences). A format change (a schema option or a manifest field), so the 4-step discipline (found in 6.6.8)
- [ ] 6.6.13 (v1.1) `ReadOnlyBlobDatabase::refresh` in place: refresh the documents with `ReadOnlyDatabase::refresh` (ADR-0060) instead of a new open, keeping the pairing with the pack refresh and the reader lease (ADR-0054); today its `database()` exposes the engine's refresh, which advances the documents without the pack (documented) (found in 6.6.6)
- [ ] 6.6.14 (v1.1) Incremental refresh across a flush of replayed records: keep the unchanged segments and rebuild only the memtables from the new `wal_start`, if a measured replica workload needs it (ADR-0060 Alternatives, found in 6.6.6)

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
- [x] 7.3.5 (v1.0) Term proximity scoring: a bonus from the minimum span, or the pair distance, of the query terms, using positions already indexed (5.3.4). The bonus must keep pruning bounds valid (MaxScore and block-max WAND, SPEC §7.2, §7.3). Done as the pair distance of adjacent `must` words, `λ · min(idf) · 1/d²` up to 5 apart in fields with positions (ADR-0036). Off by default, opt-in with `SearchPlan::with_proximity`, until 15.1.6 evaluates it; a cursor records the setting (SPEC §7.4)

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
- [x] 7.4.7 A disjunction for many clauses (relaxation expansions, SPEC §7.6: up to 50 prefix or fuzzy terms): a heap instead of a scan of every clause per document, and phrase candidates that copy positions only while starts survive; bench `search_top10/or` with 50 clauses before and after (found in the 7.4 review)
- [x] 7.4.8 Prune conjunctions and `should` clauses beside `must` with block bounds (block-max AND, required plus optional as Lucene's ReqOpt): today a threshold reaches only a term, a pure `should` disjunction and what `must_not`/`boost` wrap (SPEC §7.3, found in 7.4.4)
- [x] 7.4.10 Pruning trusts block bounds on the read path: a block whose decoded `tf` exceeds its Skip `max_tf` (or a norm below `min_norm`) drops hits silently instead of `Corrupt`; check `tf <= max_tf` at block decode, and add a fuzz arm that runs pruned `top_k_with` (both strategies) over corrupted segment bytes: `Err` or a result, never a hang (`Segment::verify` already checks it; found in the 7.4.4 review)
- [x] 7.4.11 (v1.0) Configurable tie-break: a secondary key by column (for example `updated_at` descending) before docid. A request gives one with a sort list (`["score", {"field": ...}]`, 7.5.1), which visited every match before this: done as pruning for sorts led by the score with further keys (strictly below the worst kept hit, in every view, SPEC §7.3, §7.4). Out of scope: a collection-level default, which needs a manifest version and is 32.5.1
- [x] 7.4.12 Plan `prefix` and `fuzzy`: expand each over the collection's terms once, at plan time (every view's `SearchView::terms`, ROADMAP 6.1.5), so every view scores the same terms; keep at most `max_expansions` and specify in SPEC §7.1 which terms win the cap (collection df descending, then bytes: each view's `TermEnum` df is its own, a segment's counting deleted documents, so rank by `StatisticsProvider::df`). Score every kept term with that largest kept `df` (one shared `idf`) and, for `fuzzy`, the edit-distance weight; the query bytes are not analyzed (SPEC §7.1). `SearchPlan::new` rejects them today (`SearchError::NeedsTerms`; found in 6.1.5, shares the walk with 8.2.3 and 8.2.4.2)
- [x] 7.4.13 Faster filters: a `range` scans every key of the column (~12 ns per document on `search_top10_filtered`); skip FOR blocks whose keys cannot match by their minimum and bit width (every key is at most `min + 2^width − 1`, no format change), and for a selective `must` check the column per candidate instead of building the whole bitmap (SPEC §7.3's "column predicates"), picked by cost (found in 7.4.5)
- [x] 7.4.14 Expansion `df` lookups: planning reads the collection `df` of every term each view lists, before the cap rejects it, so the memtable walks the term's postings again (its `TermEnum` just counted them) and the snapshot's capped `df` cache fills with terms that lose the cap; reuse the view's count for the memtable and bypass the cache for losing terms. Bench with a memtable and several segments (found in the 7.4.12 review)
- [x] 7.4.15 Expansion `df` from the segments' own counts: a term that loses the cap is no longer cached (7.4.14), so each plan sums its `df` over every segment again (`search_top10_expansion` +3 %) although each segment's `TermEnum` already gave its count; when the statistics are the views' segments, add those counts instead of looking each term up in every segment (found in 7.4.14)
- [x] 7.4.16 Check column filters per candidate beside other filters: a `bool` with a `keyword` filter (postings) and a `range` builds the bitmap of both today (7.4.13 checks only when every filter reads column keys); select the postings filters as a bitmap and check the column ones per candidate, and fit the per-candidate threshold (a quarter of the view, `CHECK_SHARE`) from a sweep of `must` costs on `search_top10_filtered` (found in 7.4.13). Fitted on `search_top10_filtered_sweep`: a third of the view with column filters alone (`CHECK_SHARE` 3), a fifth beside postings filters (`CHECK_SHARE_BESIDE_POSTINGS` 5); see docs/benchmarks.md

## 7.5 Sorting, pagination, facets
- [x] 7.5.1 Sort by column (asc/desc, missing handling) with top-k
- [x] 7.5.2 `offset/limit` and `search_after` cursor (SPEC §7.4)
- [x] 7.5.3 Facet counting over keyword columns with bitmap input; top-N merge across segments
- [x] 7.5.4 Result assembly: fetch stored fields, projection, blob_ref in hit
- [x] 7.5.5 Keep column dictionaries across requests: a segment's `keyword_column` decodes its whole dictionary on every sort or facet request (`SearchView::keywords`), and the memtable copies its keyword values and column keys per call; cache them per segment (beside the block cache) and per memtable view (as its norms), and bench `search_request/sort_price` and `score_facet` before and after (found in 7.5)
- [x] 7.5.6 Prune column sorts: `search_request/sort_price` visits every match (+21 % over a pruned score top-10 on 50 000 documents); skip FOR blocks whose keys cannot beat the worst kept hit (a block's minimum and bit width bound its keys, as 7.4.13) (stopping a view early when its index sort matches waits on `index_sort`, 33.6.1) (found in 7.5)
- [x] 7.5.7 Keep the memtable's column copies across requests: a request takes a new `MemtableView`, so the keyword column and column keys it copies (7.5.5) last one request, and on rayon's pool the memtable view sets the wall time of `search_keyword_columns/sort_sku` (3–5 % slower than main there in alternating runs, not explained, while 21 % faster on the calling thread); share a copy between views of the same document count, bounded with the memtable, and find that 4 % (found in 7.5.5). Done as one copy per field shared by the views of the same documents, whatever operations each saw (SPEC §10.6)

## 7.6 Parallel execution
- [x] 7.6.1 Per-segment search tasks on a thread pool; deterministic merge. A threshold shared across views (`CollectionViews::top_k`, 7.4.6) skips "at most t" only from views that win the tie (earlier ones); one from a later view must skip strictly below t (found in the 7.4.6 review)
- [x] 7.6.2 Query deadline/cancellation token
- [x] 7.6.3 Interrupts beyond the collector: a deadline or a cancellation is read only between the matches a collector is yielded (every 256) and before each view, so a matcher that walks long without a match (a phrase over common terms, a pruning disjunction skipping blocks) planning (`SearchPlan::with_terms` expansion, SPEC §7.1), building a view's matcher, and counting a facet over a large bitmap (`FacetCounter::add`) run past it; read it there too, and bench the cost on `search_top10` (found in 7.6.2)
- [x] 7.6.4 A merged threshold across views: a view shares only its own `k`-th score, so on the calling thread a later view starts from the best single earlier view instead of the `k`-th of all earlier hits merged (what sequential search used before 7.6.1); publish a merged threshold where the views finished are a prefix, and bench `search_top10` over several segments (found in 7.6.1)
- [x] 7.6.5 Interruptible memtable copies: `MemtableView::terms` tries every term of a field's hash arena under its read lock (then runs the automaton), and `try_column_keys` copies the whole column, before any interruptible reader step; copy in chunks that read the search's interrupts, so an expansion that matches nothing in a large memtable, or a filter scan over it, stops too (SPEC §7.3, found in the 7.6.3 PR review)
- [x] 7.6.6 The memtable's other whole copies for a view: `deleted` (its kill versions), `trues`, `present`, and `keys`, `keywords` and `norms` copy a whole field under one read lock with no interruptible step; copy them in chunks that read the search's interrupts, as 7.6.5 does for terms and column keys (SPEC §7.3, found in the 7.6.5 review)

## 7.7 Explain
- [x] 7.7.1 Per-hit score breakdown (per term/field) when `explain: true`
- [x] 7.7.2 Explain a page in one pass per view: today each hit builds its leaf matchers again and a filtered `bool` selects its filters over the whole view again (`search_suite/explain_filtered` 2.14 ms vs 463 µs for `filtered`); sort the view's hits by docid, build the tree once and advance it, selecting each filter once (SPEC §7.7, found in the 7.7 review)
- [x] 7.7.3 Explain from the search's own filter bitmaps: the one pass per view (7.7.2) selects a `bool`'s filters again, so `search_suite/explain_filtered` (1.34 ms) still costs about one more filter selection per view than `filtered` (569 µs); hand each view's selections from its matcher to its explainer (SPEC §7.7, found in 7.7.2)

## 7.8 Tests & benchmarks
- [x] 7.8.1 Differential suite vs. reference engine (random corpora/queries)
- [x] 7.8.2 Relevance sanity corpus with judged queries; nDCG@10 baseline recorded
- [x] 7.8.3 Criterion benchmarks: single term, AND/OR, phrase, filtered, sorted, facets
- [x] 7.8.4 (v1.0) nDCG@10 relevance regression gate in CI, building on the 7.8.2 baseline (7.8.3 stays the Criterion benchmarks). Runs in CI's `test` job (Linux, macOS) with the default plan (proximity off, ADR-0036); fails on a drop or a rise beyond one unit of the fourth decimal and prints the replacement table
- [x] 7.8.5 The reference model's norm oracle is independent of the implementation: it states SPEC §5.3.4's rounding (keep the top 4 bits of `n − 24`) instead of copying the codec, checks it against the SPEC's properties (`q <= n`, exact below 40, less than `(n − 24) / 8` low, monotonic), and checks the engine's codec against it for every length up to 2^17 and by proptest to 2^31 (found in the 2026-11-02 audit)
- [x] 7.8.6 The differential suite compares ranked order to the reference directly (every pair of hits, within the score tolerance). Made explicit only: each score within tolerance of the reference plus the engine's own order already implied it, so it adds no detection power today (found in the 2026-11-02 audit)

---

# M8 — Zero-results avoidance (`persia-engine::relax`)
Deps: M7. **Key differentiator — treat quality here as a product feature.**

## 8.1 Framework
- [x] 8.1.1 `RelaxPlan` = ordered levels with per-level query transforms; configurable via schema/query (SPEC §7.6)
- [x] 8.1.2 Orchestrator: run L0, evaluate `min_results`, escalate, stop or fill
- [x] 8.1.3 Ordering across levels: the level leads the rank key (every stricter hit above every looser one at any score; an `f64` score offset cannot give that for every score, ADR-0037), a monotone rank score per hit, and paging (offset, cursor) across levels
- [x] 8.1.4 Budget (`budget_ms`) and `truncated` flag
- [x] 8.1.5 Response metadata struct (`level`, `applied`, `original`, `effective`, `corrected`)
- [x] 8.1.6 `minimum_should_match` on `BoolQuery` (SPEC §7.1) for L2 `drop_terms` (found in the 7.1 review)
- [x] 8.1.7 Source span and original text of each compiled clause in `Compiled`, for `original`/`effective`/`corrected` and the L3/L4 length rules (found in the 7.1 review)
- [x] 8.1.8 Relaxed queries within `QueryLimits`: cut expansions to the remaining budget, report it in `applied` (SPEC §7.1.3, found in the 7.1 review). Count the earlier steps a step excludes too (its `must_not`, ADR-0037 item 2), whose expansions planning repeats (found in the 8.1 review)
- [x] 8.1.9 Facets under relaxation: specify in SPEC §7.5 which matches a facet counts when the cascade runs (§7.6: the strict query, the level returned, or the union `fill` collects) and implement it with M8 (found in the 7.5 review). 8.1.2 counts the union of the steps built as an interim rule (ADR-0037 item 4); with `fill` a deeper page may count more steps Done as: a facet counts the steps a page holding every hit builds, through its hit step, each document once; a `fill` page with facets counts the steps past its own without listing them (SPEC §7.5, ADR-0061; `db/relaxed/tests` facet tests and the facet property over `fill` in `db/relaxed/tests/facets.rs`). Bench `search_relaxed/facets/fill` (docs/benchmarks.md): a first `fill` page with facets pays for the later steps' counts.
- [x] 8.1.10 Make SPEC §7.6's response example agree with the cascade: `applied` lists every step built in order, and whether L4 `fuzzy` builds on the full query or on the last `drop_terms` step (the example's `effective` keeps both words) is for 8.2 to decide; settle it with the first of 8.2.2 or 8.2.4 (found in the 8.1 review)
- [x] 8.1.11 Cursor pages with facets search every step before the cursor's again only to count it, spending the budget (SPEC §7.6, a page may be truncated before any hit): carry the counts with the cursor, or count once per snapshot and query, and bench a deep facet page (found in the 8.1 review) Done as: a per-snapshot cache of facet counts (`db::relaxed::facet_cache`, at most 32 counts and 1 MiB, LRU by a use counter) keyed by memtable version, query, compilation, the soft filters the filter step may drop, settings, limits and facets; later pages read it and search only their own steps (SPEC §7.5, ADR-0061 item 4; bench `search_relaxed/facets/deep`, docs/benchmarks.md). Only a page from the top whose counting steps the budget did not cut keeps its counts (a cursor page never does: its cursor may come from a truncated page); `facets.rs` `a_cursor_page_after_a_truncated_page_keeps_no_counts`, `a_budget_passing_in_a_counting_step_leaves_the_page_whole_and_keeps_no_counts`.
- [x] 8.1.12 Scores can overflow to `+∞` (`constant_score` near `f64::MAX` under a `boost`, or a `bool` summing large ones; SPEC §7.2 allows each): decide whether to clamp to `f64::MAX`, reject the query, or keep `+∞`, and how JSON responses (§12), which have no `Infinity`, carry it. The cascade and `rank_score` already order `+∞` correctly (found in the 8.1.3 review) Done as: every node saturates its score at `f64::MAX` (`score::saturate`: `bool` sums in the conjunction, disjunction, required-plus-optional, `MaxScore` and block-max WAND matchers, `boost` products, and the same nodes of `explain`), so no hit, rank score or explanation carries `+∞` and JSON states a plain number; bounds stay raw upper bounds (SPEC §7.2, `tests/score_saturation.rs`).
- [x] 8.1.13 Synonym expansions within `QueryLimits` (SPEC §7.1.3): the cascade's fitting (`query::StepLimits`, ROADMAP 8.1.8) cuts only `prefix` and `fuzzy` `max_expansions`, so an L1 step over `max_terms` or `max_clauses` is skipped whole. 8.2.1 must mark its synonym alternatives as cuttable (least important first) so the fitting drops some instead (found in 8.1.8)
- [x] 8.1.14 Each 8.2 level renders its steps' `effective` text from the compiled clauses' spans (`relax::Step::effective`, ROADMAP 8.1.7): a level that leaves it `None` makes the response's `effective` absent for a compiled query (found in the 8.1 batch review). L2 renders it (8.2.2.2), L1 too (8.2.1: L0's text) Done as: every level already rendered its steps' text from the clauses' spans; `db/relaxed/tests/effective.rs` checks every step of every level (L0 to the filter step, `prefix:last` and `fuzzy:1` included) states one for a compiled query, and each level as the hit step gives the response its `effective`.
- [x] 8.1.15 Filters in the reference model (`tests/reference/search.rs`), so `minimum_should_match` beside a `filter` (and every filter path) is tested differentially, not only by hand and pruned-against-exhaustive (found in the 8.1 batch review)
- [x] 8.1.16 Compile L1's synonym alternatives lazily, or cache them per default fields: a query with a 50-alternative match compiles in about 41 µs against 2.5 µs without synonyms (bench `compile/synonyms`), mostly analyzing the alternatives again, paid even when L0 has enough hits (ADR-0039 consequences; found in 8.2.1) Done as: a cache, not lazy compilation (ADR-0058: `Compiled::synonyms` stays public and unchanged): `QueryCompiler` keeps each synonym term's compiled alternative per set of default fields, the last 4 sets, filled once per term (`OnceLock`, shared across threads). A set's slots are allocated per group on first use (a 24-byte cell per group up front, a 16-byte slot per term of a group a run drew on; ADR-0058 "Memory"), and a new set is built outside the lock. Bench `compile/synonyms` 37.7 → 19.4 µs; new `search_relaxed/l0/compiled_synonyms` (compile + search with enough L0 hits) 516 → 498 µs against 476 µs without synonyms (docs/benchmarks.md).
- [x] 8.1.17 How a request's `filter` joins a compiled query (SPEC §7.1: the root `bool`'s `filter`): a collapsed compilation (one clause, one part) wrapped with a filter makes `Compiled::parts` read the whole root as clause 0, filter included, for L1 and L2 alike; give `Compiled` the filter, or rebuild the root from the clauses (found in the 8.2.1 review)
- [x] 8.1.18 L1 in the differential suite: the reference model expands synonyms as SPEC §6.3.2 does, and a property checks that L1's step matches every document L0 matches, under every cut (found in the 8.2.1 review)
- [x] 8.1.19 L3 in the differential suite: the reference model expands `prefix` as SPEC §7.1 does (the 50 terms of highest collection `df`) and builds L3's step by §7.6's rule, so invariant 5 of `tests/differential.rs` runs the collection's levels again (pinned to L2 since L3 builds a step for one programmatic term). Once a schema can name a registered analyzer (6.6.8), test through a database that a field stacking tokens (`edge_ngram`) is not expanded (`db::relaxed` tests give the memtable such an analyzer today) (found in 8.2.3)
- [x] 8.1.20 L3's last term is expanded whatever its length, so a one-letter last word (`w`) walks its field's whole term dictionary in every view before keeping 50 by `df` (bench `search_relaxed/l3`: about 23 ms over 4 segments of the bench corpus, against 1.4 ms for an L2 step). Bound it: a minimum length for the last term too (a SPEC §7.6 decision), a per-view walk budget, or `df`-ordered term walks (found in 8.2.3) Done as: a minimum length of 2 characters for the last term (Unicode scalar values of the analyzed term, as the 3-character rule counts; SPEC §7.6, ADR-0057, amending ADR-0040 item 4); a one-character last term stays exact and builds no `prefix:last` tier step. The differential reference applies the same rule (one-letter query words are generated since 8.1.27); bench `search_relaxed/l3` (`w3 w`) 22.6 ms → 26 µs (L3 skipped, L5 reached), new `l3/two_letters` (docs/benchmarks.md).
- [x] 8.1.21 L4 in the differential suite: the reference model expands `fuzzy` as SPEC §7.1 does (the optimal string alignment distance over characters, the 50 terms of highest collection `df`, one shared `idf` and the edit-distance weight) and builds L4's step and corrections by §7.6's rule, so invariant 5 of `tests/differential.rs` runs L3 and L4 beside L2. The reference has no `prefix` or `fuzzy` query yet, so it lands with 8.1.19's expansions; until then `tests/relax_fuzzy.rs` checks the step against an oracle (found in 8.2.4)
- [x] 8.1.22 A last step that adds no hit still states its `effective` and, at L4, its `corrected` (SPEC §7.6: the last step's text): with the collection's levels, `red shoez` over `red shoes` reports `corrected: shoez → shoes` though every hit is L2's `red` (`tests/relax_fuzzy.rs` pins it). Decide whether the response states the last step that found a hit instead, keeping every page of a query alike (a cursor page may not search the step), and the same for L3 (found in the 8.2.4 review)
- [x] 8.1.23 The stacking-analyzer part of 8.1.19, which waits for a schema that can name a registered analyzer (6.6.8): test through a database that L3 and L4 do not rewrite the terms of a field whose analyzer stacks tokens (`edge_ngram`, SPEC §6.3.1, §7.6), with the differential reference skipping such a field as the engine does (`db::relaxed` unit tests give the memtable such an analyzer today) (split from 8.1.19) Done as: `tests/relax_stacking.rs` names a registered `edge_ngram` analyzer (`grams@1`, 6.6.8) in a schema beside a `standard` field and checks through a database, against a reference that rewrites only the fields whose analyzer does not stack tokens, that L3 expands and L4 fuzzes (and corrects) only `title`, over random corpora and words, with the same field under a non-stacking analyzer as the control (both levels covered); with the stacking field as the only default field neither level builds a step. The differential suite's own reference skipping such a field is split off as 8.1.28.
- [x] 8.1.24 L1 over default fields that analyze synonym groups differently, in the differential suite: both of its text fields use `whitespace`, so the fields' union of alternatives (in group then term order, a later field's term before an earlier one's), the overlap drop, alternatives that compile away (duplicates over the other field), stopword gaps and words of several tokens are checked by hand only (`tests/reference/synonyms.rs` unit tests, `tests/relax_synonyms.rs`). Give `body` an analyzer the reference can model (`standard` lowercases, and on the lowercase vocabulary tokenizes as `whitespace` does) and add case variants to the synonym terms (found in the 8.1.18 review)
- [x] 8.1.25 Stopword gaps in the differential suite: neither of its analyzers removes a token (`whitespace`; `standard` has no `stop` filter, and every built-in with one also stems), so the rule that a gap must line up (SPEC §6.3.2) is checked by hand only (`tests/relax_synonyms.rs`, `tests/reference/synonyms.rs`, `persia-analysis` `tests/synonyms.rs`). Once a schema can name a registered analyzer (6.6.8), give a default field one the reference can model (`standard` and an English `stop` filter) and stopwords to the vocabulary (found in 8.1.24) Done as: `body` of the differential suite is `standard-stop@1`, a registered `standard` + English `stop` analyzer (6.6.8) the reference models (stopwords removed, positions kept, positions taken counted); the analysis self-check also compares the positions taken and the stopword flag. `stopword_gaps_match_the_reference` runs invariants 1–5 with `the`, `of` or `be` put into every text and every synonym term of several words, and covers a `body` synonym match across a stopword gap and L2's hit step over a stopword clause.
- [x] 8.1.26 A run whose alternatives all compile away, in the differential suite: over two default fields with positions an alternative compiles to the run's own query only if it analyzes as the run in every field, and then no field reports it (the compiler's unit test `an_alternative_that_compiles_to_the_run_itself_is_left_out` checks it alone). Reaching it needs a default field without positions compiled as the only one (`fields`), where a phrase is its terms required, which changes the phrases of invariants 1 to 4 (found in 8.1.24). Done as: shown unreachable through `compile`, for any schema and analyzer, which includes the route above (a default field without positions compiles a word of several tokens to a phrase or its terms required, never to the run's `combined` leaves). The argument is in the header of `crates/persia-engine/tests/differential.rs` (invariant 5); the compiler's unit test `an_alternative_that_compiles_to_the_run_itself_is_left_out` covers the guard alone, and the batch review verified the argument independently (owner's decision)
- [x] 8.1.27 The differential suite's coverage asserts hold for its fixed seed only: on other seeds (`PROPTEST_RNG_SEED=1`, `2`, `3`) some step kinds get no case, on `main` too (`synonyms compiled cut` then, made deterministic in 8.1.24; `prefix:last hit`, `fuzzy:cut hit` and 8.1.24's `prefix hit over a last word the fields split apart` and `fuzzy hit step beside a split word not corrected` still come in a few cases), so a generator change reshuffles which key falls to zero. Build the thin ones on purpose, as the compiled-cut check does (a compile at the bound where it binds), rather than tuning weights for one seed (found in 8.1.24) Done as: the thin kinds are built on purpose in every case, not by weights: one synonym writing of each shape per case (the five kinds of 8.1.24, a title-only one of no other group's term), each shaped term compiled alone after the case's last operation (runs checked against the reference) and one or two searched; texts built from the live documents after the last operation (`<word> <prefix>` for `prefix:last`, `<word>-<prefix>` for a split last word, `<typo>-<typo>` with a `q` for L4 beside a split word not corrected); and each `fuzzy` step that finds a hit searched again at one expansion below what it needs (every fourth text and operation), for `fuzzy:cut hit`. One-letter query words (`c`) are generated for 8.1.20, with their own coverage key. Every key holds on `PROPTEST_RNG_SEED` 1, 2, 3 and the fixed seed (lowest counts 19 for `fuzzy:cut hit`, 28 for the split-word L4 hit, 39 for a later-field-only match).
- [ ] 8.1.28 (v1.1) A default field whose analyzer stacks tokens in the differential suite: the reference models an `edge_ngram` field's index (stacked grams at one position, counted in the field length and `tf` as the engine counts them) and skips it at L3 and L4 as `tests/relax_stacking.rs` does (found in 8.1.23)
- [ ] 8.1.29 (v1.1) A step cut to fit the limits (SPEC §7.1.3) that lowers an expansion below an excluded one it subsumed counts that one again in full, and is cut with no credit (ROADMAP 8.2.12): cut the caps that subsume an exclusion no lower than it first, so such a step keeps more of its own expansions (found in 8.2.12)
- [ ] 8.1.30 (v1.1) The differential suite generates hard filters beside its soft one (the case's filter is passed as `soft_filter` or marked soft today, so its reference of L6 is unchanged): hard filters are covered by `tests/relax_hard_filters.rs`'s own oracle, not by the reference model (found in 8.2.13)
- [ ] 8.1.31 (v1.1) A relaxed response says when its facets are partial: the budget passed in a step a page searched only to count its facets (SPEC §7.5), so the page is whole but the counts miss later steps; today nothing flags it (e.g. a `facets_truncated` flag beside `truncated`, found in the 8.1.9 batch review)
- [ ] 8.1.32 (v1.1) Count a `fill` page's facets with fewer passes: L5 covers every earlier step (`covers_earlier`), so the union of the steps through L5 is L5's selection, and counting that once, unscored, gives the same counts as searching L2's later rungs, L3 and L4 with every match visited (bench `search_relaxed/facets/fill`: 1.39 → 12.8 ms for a first `fill` page with facets since 8.1.9; ADR-0061 Consequences, found in the 8.1.9 re-review)
- [x] 8.1.33 (v1.0) A cascade the budget truncates can still answer with zero results (SPEC §7.6 "Budget", pillar 3). The budget is read from L0's end, and once it has passed the cascade stops before the next level: a query whose L0 found nothing and whose budget passes before L5 `fallback` answers `truncated: true` with no hit, though L5 would have filled the page (verified in `search_relaxed`, which breaks before a level and before a step once the budget has passed, and in `db/relaxed/tests.rs`, `l0_is_never_cut_and_a_budget_passed_after_it_stops_before_the_next_level`). Decide in SPEC §7.6 with an ADR: when the page would otherwise be empty and the plan has L5, L5 `fallback` still runs past the budget, bounded by the request's own deadline and cancellation; the steps the budget skipped stay skipped and the response stays `truncated: true`. [T] With a budget that passes right after an empty L0, the page holds L5's hits and states `truncated: true`; with a deadline that passes too, the search fails as the deadline's error; a plan without L5 (`fallback: "none"`) still answers the empty truncated page (found in the wave 7 planning) Done as: when the budget left the page empty, a plan with L5 runs L5 past the budget, bounded by the deadline and cancellation; the page stays truncated, skipped steps stay skipped, no L5 means the empty page; ADR-0066, SPEC §7.6 Budget.
- [x] 8.1.34 (v1.0) A cursor from the page L5 filled past the budget pages the next page exactly (SPEC §7.4, §7.6 "Budget"; ADR-0066 Consequences). The rescued L5 takes the step index after the steps completed, not the index L5 has in a cascade the budget does not cut, and a hit's cursor records that index: the next page, when its budget does not pass at the same point, rebuilds other steps up to it and searches a step that is not L5 after the cursor, so it may repeat or skip hits. Record that the cursor's step is L5's (a cursor token version, ADR-0062 item 11), or have the next page rebuild the same steps. [T] With a budget that passes after an empty L0 on the first page and not on the second, the pages by cursor are the pages by offset (found in 8.1.33) Done as: cursor token v2 flag 32 (`StepCursor::fallback`): after a cursor of L5's step the cascade builds the steps before it and runs L5 at that step after the cursor, past the budget if needed; L5 never takes another level's cursor step; ADR-0066 item 5, SPEC §7.6 Budget.

## 8.2 Levels
- [x] 8.2.1 L1 synonyms: expand terms with synonym groups (one step; runs of whole clauses, alternatives as phrases, cut in group order; ADR-0039)
- 8.2.2 L2 drop_terms
  - [x] 8.2.2.1 Term importance ordering (IDF + position + stopword flag)
  - [x] 8.2.2.2 `minimum_should_match` ladder 100/75/50/1
  - [x] 8.2.2.3 Single-term queries skip L2
  - [x] 8.2.2.4 Exclude only the last searched step of a nested level: L2's steps are nested (L0 ⊆ 75% ⊆ 50% ⊆ 1, same `must_not` and `filter`), so excluding the latest one gives the same hits as excluding them all (ADR-0037 item 2), and stops a long query losing L2 steps to `max_terms` (step j uses about (j + 1)·n terms) and re-planning every excluded step (bench `search_relaxed/l2_every_step`). SPEC §7.6 and §7.1.3 wording; L1's steps are not nested and stay excluded (found in the 8.2.2 review)
  - [x] 8.2.2.5 Decide whether a term no document holds (`df` 0, often a typo) counts as the most important for L2's `effective`: today it is reported kept unless it is a stopword, though no hit of the step holds it (`purple red shoes` → `purple red`). A stopword is still dropped first. Options: rank `df` 0 last, or leave it to L4's corrections (8.2.4.3) (found in the 8.2.2 review). Decided: a `df`-0 term is the least important, below a stopword; L4 cannot change the text of L2's own steps, and it runs only when L2 found too few hits (it can replace the response's `effective` as the last step, 8.1.22) (SPEC §7.6, ADR-0042 item 8)
- [x] 8.2.3 L3 prefix expansion (FST range, cap expansions, rank by df)
- 8.2.4 L4 fuzzy (depends on 6.1.5)
  - [x] 8.2.4.1 Levenshtein automaton builder (distance 1–2, transpositions). Since 7.4.12, `search::Expansion` walks the optimal string alignment table row by row (an allocation per byte); compile it to a DFA, or at least keep only the band of 2·distance + 1 columns that can stay within the distance (each byte is O(query length) today, and a query term may be 32 KiB), if `search_top10_expansion/fuzzy` shows planning matters (found in the 7.4.12 review)
  - [x] 8.2.4.2 FST ∩ automaton term enumeration with cap + df ranking
  - [x] 8.2.4.3 "Did you mean" corrected-query synthesis
  - [x] 8.2.4.4 Length-based distance policy (SPEC §7.6 L4)
- [x] 8.2.5 L5 fallback: static_rank / recency top-N; clearly flagged in response
- [x] 8.2.6 Optional filter relaxation (`last_resort`) with explicit opt-in
- [x] 8.2.7 (v1.0) Typo and exactness as a ranking signal: within a relaxation level, 0 edits rank above 1 edit, then 2, before BM25F; exact ranks above stemmed, then prefix (8.2.6 remains optional filter relaxation)
- [ ] 8.2.8 (v1.1) A durable per-document index order for L5 `fallback`'s recency (SPEC §7.6, ADR-0044): today recency is index location (memtable, then segments newest first, docids descending), which a merge reorders, as its output is added after the kept segments (§8.5). Record each document's write order (for example the commit `seq` of its write, as a hidden column written at flush and copied through merges) so recency is write time and merge-stable. An on-disk format change: SPEC, version or feature flag, ADR and golden fixture (found in 8.2.5)
- [ ] 8.2.9 (v1.1) L5 `fallback` by static rank reads every candidate's column key when the FOR bounds (the minimum plus `2^width − 1`, SPEC §5.2.1) do not separate blocks: values spread over their range (the bench's prices) give every block the same bound, so `search_relaxed/l5/static_rank` reads all 50 000 keys (886 µs, ADR-0044). Store an exact maximum per block, or a per-segment order by static rank written at flush (both on-disk format changes), or cache L5's order per snapshot. Each block visited, and each window a recency walk reads back, builds the step's matcher again, which looks up the postings of every earlier step it excludes (after the default levels, the L2 ladder and the L3 and L4 expansions): measure L5 after the default levels too (found in 8.2.5)
- [ ] 8.2.10 (v1.1) Exact above stemmed within a level (ROADMAP 8.2.7, ADR-0046 item 6): a word matched as typed ranks above one matched only by its stem. The index holds the stems alone, so this needs an unstemmed form per token (for example a hidden exact-form term beside each stemmed one at flush, copied through merges): an on-disk format change (SPEC, version or feature flag, ADR and golden fixture), then a tier step of L0 or L1 that requires the exact forms (found in 8.2.7)
- [ ] 8.2.11 (post-v1) Finer tiers by a per-hit key (ADR-0046 alternatives): total edits over the words, then the number of completed words, and per run at L1 how many runs matched as written, a leading key of score-led sorts with tier-aware pruning. Do it only if judged queries show a gain over the tier steps, which tier by the most edits any one word needs (found in 8.2.7)
- [x] 8.2.12 Make L4's tier steps nested by construction, so later steps exclude only the latest one (ROADMAP 8.2.2.4). For example, `fuzzy` keeps `fuzzy:1`'s terms beside its own 50, or `fuzzy:1` keeps only the terms within one edit that `fuzzy` keeps. Today `fuzzy` excludes `fuzzy:1`, whose expansions count again against `max_expansions` (SPEC §7.1.3), so a query with many long words reaches `fuzzy:cut` or `fuzzy:skipped` sooner than before the tiers (found in 8.2.7) Done as: a `fuzzy` keeps its expansions fewest edits first, then by `df` (SPEC §7.1), so `fuzzy:1` keeps only terms `fuzzy` keeps and L4 is a nested level (later steps exclude `fuzzy` alone); `query::StepLimits` counts an excluded `prefix` or `fuzzy` the step subsumes once (SPEC §7.1.3), so `fuzzy` beside `fuzzy:1` counts as before the tiers (`relax_fuzzy.rs` `tier_steps_nest_so_many_long_words_are_not_cut_sooner`, `query::fit` unit tests, the reference model's keep order, nesting and credit in `tests/differential.rs`); ADR-0046 item 7; bench rows in docs/benchmarks.md.
- [x] 8.2.13 A request key for filters that never relax, beside the ones `filters: "last_resort"` may drop (ROADMAP 8.2.6, ADR-0047 item 3): today the filter step drops every filter of the root `bool`, so a request whose filters enforce tenancy or permissions must not opt in. Decide the request JSON (for example a hard `filter` and a soft `filter`, with 10.1 and 11.x), keep the hard ones in the filter step and in L5, and test that no hit of any level breaks a hard filter (found in 8.2.6) Done as: `filter` is hard and `soft_filter` (same grammar) is what `last_resort` drops (`Compiled::with_soft_filter`, `RelaxedQuery::with_soft_filter` for a programmatic root, whose filters are hard unless marked; request JSON parsing is 10.1.10); L6 drops the soft filters alone and builds no step without one, L5 and every word level keep both (`tests/relax_hard_filters.rs` property: no hit of any level, page or cursor breaks a hard filter; `relax/filters` unit tests; ADR-0056). A caller's own `with_query` root filters are hard (`Compiled::droppable`); L5 keeps a wrapped caller root's `must_not` and `filter` and builds no step when a dropped clause holds a filter, and L2 none for a programmatic root whose `must` holds one (`relax_hard_filters.rs` by-hand cases and the caller-filters arm of the property).
- [x] 8.2.14 L5 and the filter step read a compiled query's filters from `Compiled::root`, which is `None` once a caller replaces `Compiled::query` with a query that does not hold its clauses' parts: L5 is then `match_all`, so a filtered query falls back without its filter under `filters: "never"` (SPEC §7.6 states the root rule). Keep the request filter beside the compilation (or make `query` private), so no level can lose it, and test a replaced query with a filter (found in the 8.2.6 review)
- [x] 8.2.15 Let the word levels keep a required clause a caller adds beside a compiled query's clauses (`Compiled::with_query`): today such a root detaches the compilation (ADR-0048 item 3), because L1–L4 rebuild `must` from the clauses alone and would relax the added clause away unlisted, so the cascade then builds no word step for it. Carry the extra `must` entries on every step, or state them in `effective` (found in the 8.2.14 review) Done as: a root whose `must` starts with the clauses' parts keeps them attached; the entries after them (`Compiled::extra_must`) are carried unchanged by every step (L1 units, every L2 rung's `must`, L3/L4 after the rewritten clauses, L5's selection, the filter step's root) and not rendered in `effective` (`db/relaxed/tests/extra_must.rs` per level; ADR-0056 item 2, ADR-0048 item 3 amended).
- [ ] 8.2.16 (v1.1) `Compiled::with_extra_must`: add required clauses beside a compiled query's clauses without rebuilding its root by hand, since passing back a clone of `compiled.query()` to `with_query` makes the request's soft filters the caller's own (hard, so no filter step), in place and wrapped, keeping the parts a level relaxes attached (ADR-0056, found in the 8.2.13 re-review)

## 8.3 Controls & API
- [x] 8.3.1 Per-query `relax` options override; per-collection defaults; `relax:false`
- [x] 8.3.2 `fill` mode (continue to fill `limit` from looser levels). The engine's cascade fills since 8.1.2 (`RelaxOptions::fill`); what remains is the request option of 8.3.1 and its tests end to end
- [x] 8.3.3 Metrics: relaxation level histogram, % queries relaxed, % still empty Done as: atomic counters per collection, shared by its snapshots and read as a `RelaxationStats` snapshot (queries without a cursor, relaxed, still empty, per-level hit steps; `relaxed_ratio`, `empty_ratio`, `levels`), per collection and summed for the database (`Database::relaxation_stats`, a dropped collection's counts kept); one `tracing` debug event per relaxed search (collection, level, applied, hits, truncated); a budget-truncated cascade counts by the steps it ran. SPEC §7.6 "Metrics", §12.4. M11's exporter reads the struct; a read-only handle's database sum is 11.4.8.
- [x] 8.3.4 `explain: true` also states which relaxation level a hit matched at (SPEC §7.7; the score breakdown is 7.7.1)

## 8.4 Quality evaluation
- [x] 8.4.1 Synthetic typo/term-drop query generator (from corpus) with known ground truth Done as: `persia_testutil::synthetic` (dev-only, no dependency): a seeded SplitMix64 `Rng`, `Corpus::synthetic` (pronounceable vocabulary, skewed word draws) and `Corpus::from_texts` (duplicate ids rejected), and `QueryGenerator` drawing typo (one OSA edit, at least 4 letters, held by no document), term-drop (a word no document holds beside the intended ones) and both, each query with its seed and ground truth (`relevant`: the documents holding every intended word). Every draw loop is bounded (a degenerate corpus gives fewer queries, never a hang).
- [x] 8.4.2 Metric: zero-result rate before/after relaxation; precision of first relaxed hit Done as: `crates/persia-engine/tests/relax_quality.rs` measures a synthetic suite (8.4.1) through the cascade, per kind of query, per kind split into one-word and longer queries, and overall: the zero-result rate before relaxation (L0 alone), after it, and the precision of the first relaxed hit (the share of first hits found after L0 that hold every word the query meant). The budget runs on a frozen clock, so the measure is deterministic; the metrics are checked on outcomes by hand and against `RelaxationStats` (8.3.3).
- [x] 8.4.3 Regression gate: zero-result rate on synthetic suite must be ≤ threshold Done as: three checks on the synthetic suite (SPEC §16.1): (a) no query without a hit with the default settings, (b) at most 2% with `fallback: "none"`, (c) first-relaxed-hit precision at most 2 points below `tests/relax_quality/baseline.txt` (compared in hundredths; a changed query count fails as a changed suite). Fast suite (200 documents, 150 queries, 70.67%) in `cargo test`, slow suite (1 000 documents, 600 queries, seeds 1–5, 61.83–65.67%) under `slow`. Queries mean 1 to 3 words so L4 counts. The gate fails with L2 or L4 left out, with both and the fallback left out, and below its baseline; it cannot see L1, L3 or L5 yet (8.4.6). `tests/relax_zero_results.rs`: a hit for every query with the defaults over 5 corpus shapes, 9 query shapes and 3 layouts (memtable, segments, WAL reopened), seeds 1–3 (1–10 under `slow`).
- [x] 8.4.4 Document tuning guide (`docs/relaxation.md`) Done as: `docs/relaxation.md`, checked against SPEC §7.6 / §6.2.3 / §7.1.3: the cascade, a table of the levels (what each applies and when it builds no step), the settings with their defaults and ranges, choices per use case (with the slow suite's measured trade-off of `drop_terms` on typo queries), reading `relaxation`, the metrics (8.3.3), the quality gate and its blind spots (8.4.3), and the measured costs from docs/benchmarks.md.
- [ ] 8.4.5 (v1.1) Multi-word typo queries are answered by L2 dropping the misspelled word before L4 can correct it (found in 8.4.3): on the slow synthetic suite (5 seeds) the first relaxed hit of a 2–3-word typo query holds every intended word 51.6–61.8% of the time with the default levels, against 100% without `drop_terms`; but without `drop_terms`, term-drop queries fall to the fallback (0.7–1.4%, against 75.5–83.6% with it), and a one-word query with an added word and a typo keeps the added word (0% with the default levels, every seed). Decide (ADR) whether L4 should run before L2's lower rungs, or a rung should try the corrections of the words no document holds, and gate the change with `relax_quality`'s baseline
- [ ] 8.4.6 (v1.1) Synthetic queries the gate cannot see yet (found in 8.4.3): synonym queries (a corpus with synonym groups, for L1), prefix queries (a last word cut short, for L3 apart from L4) and queries only L5 answers (every word unheld and beyond L4's reach), so the gate fails with L1, L3 or L5 left out too

---

# M9 — Blob store (`persia-blob`)
Deps: M1, M2.

## 9.1 Core model (SPEC §9)
- [x] 9.1.0 ADR: blob upload staging and expected-id API (SPEC §18.8); update SPEC §9.3/§9.4/§9.6
- [x] 9.1.1 Content hasher per ADR-0002 producing `BlobId` (type from 1.10.1), chunking parameters
- [x] 9.1.2 Streaming chunker (`Read` → chunks with CRC and running content hash)
- [x] 9.1.3 Blob manifest (chunk list) encode/decode

## 9.2 Local pack file
- [x] 9.2.1 Pack header/superblock + `BlobChunk`/`BlobIndex` frames (SPEC §9.3)
- [x] 9.2.2 Append blob (dedup check by id), fsync policy
- [x] 9.2.3 Read full / range via chunk index (mmap-backed)
- [x] 9.2.4 Recovery: torn tail truncation, index rebuild by scan
- [x] 9.2.5 Fuzz pack reader
- [x] 9.2.6 Sweep stale pack temporary files `<name>.persia.blobs.tmp.*` with the container's `sweep_stale_files` (SPEC §9.3; found in 9.2.2). Done as a sweep at each read-write open of the pack, by those rules but stricter (regular files, lowercase hex only, one directory sync); the container's own sweep listing these names is 9.2.10
- [x] 9.2.7 Enforce one pack writer: open the pack read-write only with a proof of the container's writer lock, so two read-write `PackStore`s of one file cannot exist (ADR-0049; found in the 9.2.1 review)
- [x] 9.2.8 Pack corruption errors carry the offset of the bad slot or frame (found in the 9.2.1 review)
- [x] 9.2.9 Pack read path: one read per chunk (the frame length from the manifest), one CRC pass instead of the frame's and the chunk's, and no zero-fill or copy; bench `pack` before and after (found in 9.2.3 and its review). Done as one read and one CRC pass per chunk and no copy (a 16 MiB read 27.7 → 5.1 ms, `pack` bench). The read buffer is still zero-filled, at most once per `get_blob` stream and once per `read_range` or `read_chunk_source` call; removing that zero-fill is 9.2.11
- [x] 9.2.10 (v1.0) `Container::side_files` and `persia container sweep` also list and remove the pack's stale `<name>.persia.blobs.tmp.*` files (SPEC §9.3; found in 9.2.6) Done as: `side_files` lists the pack's temporaries (the pack sweep's strict rule), its lock (`SideFileKind::Lock`) and lease directory (`SideFileKind::Leases`, never removed); `sweep_stale_files` and `persia container sweep` remove the temporaries only while holding the pack's writer lock, with one directory sync; a non-regular lock entry keeps every temporary
- [x] 9.2.11 (v1.0) `read_chunk_checked` over the pack in one CRC pass without costing other stores a copy; the `pack` bench gains a mapped-read case once a bench can make a `MapPermit` (found in 9.2.9) Done as: a provided `BlobStore::read_chunk_checked` (the default unchanged, the pack overriding it with `read_chunk_source`'s one CRC pass); the `pack` bench gains mapped cases and `chunk_checked_4MiB` (ADR-0049 amendment for its `MapPermit`)
- [x] 9.2.12 (v1.0) [T] A pack rewrite with mapped reads on: the window remap after the swap (the copying stand-in is private to `pack::store::mapped`; found in 9.5.2) Done as: unit tests over the copying stand-in (the remap onto the new file; a failed remap leaves no window) and a doctest with a real `MapPermit`
- [x] 9.2.13 (v1.0) A read-only local pack reopens when `refresh` returns `Error::PackReplaced`: today that handle keeps the file it opened, and the bytes it mapped, only until the error, and nothing reopens or remaps (SPEC §9.5, §18.10; found in the 9.5 review). The same gap is a local reader lease, when a reader must keep a blob the writer has stopped referencing. Done as: the reopen on `PackReplaced` is done (#111, `PackStore::reopen`); the local reader lease part is tracked by 9.2.16 Verified in wave 5: `PackStore` itself needs no reopen or remap of its own (its refresh remaps after new frames, a writer's rewrite swaps and remaps, 9.2.12, and a replaced handle's file and mapping stay valid until dropped); `reopen` starts with mapped reads off, documented
- [x] 9.2.14 (v1.0) The pack open bounds a `BlobIndex` frame before reading it, as the checksum scan does: `recover` reads index frames whole, bounded only by the file length, so a crafted pack can make the open allocate up to that (CLAUDE.md: corrupt input is an error, never an OOM; found in the 9.5.3 review) Done as: a bound per frame kind (a 64 MiB chunk frame, an index naming every chunk frame before it) checked before any frame is read into memory, in the scan of the committed frames (open and refresh) and in the roll-forward (an oversize tail frame is truncated, only streamed by the checksum pass), shared with the checksum scan; the fuzz target `pack_reader` gains arbitrary frames after a valid prefix (mode bit 4), and `pack_frame_bounds.rs` measures the longest read
- [x] 9.2.15 (v1.0) A writer lock before the pack exists (Unix): two read-write handles opened while no pack file exists can both live, and the one that loses the race to create it fails at its first write and is poisoned; lock beside the pack, or create it under the container's lock (ADR-0053; found in the 9.2.7 review) Done as: `<name>.persia.blobs.lock` beside the pack on every platform (`Vfs::lock_beside` on Windows, `try_lock` on the same file elsewhere), taken before the open sweeps, recovers or creates anything and held until drop; the per-file locks on the pack, its temporaries and rewrites are removed (ADR-0053 amendment; cross-process tests in `pack_lock_processes.rs`)
- [x] 9.2.16 (v1.0) A local reader lease: a read-only reader in another process is not in the GC mark, so a document it opened before a delete whose blob the sweep then collected reads as `BlobNotFound` once its refresh reopens the rewritten pack; the writer must keep the blobs a live reader may still reference (SPEC §9.3, §9.5; the lease part of 9.2.13, whose reopen on `PackReplaced` is done; found in wave 4) Done as: reader leases (ADR-0054): the `pack::lease` codec and protocol in persia-blob, the GC rule in `persia::internal::gc` (held back unless every live lease is open at the committed seq), `ReadOnlyBlobDatabase` takes its lease before its documents and advances it on refresh, `persia verify` takes it first; golden files in `tests/fixtures/blob-lease/v1/`, fuzz target `reader_lease`. A snapshot kept across a refresh is 13.7.4, the two-pass sweep 13.7.2, the operator CLI 13.7.1

## 9.3 Remote layout
- [x] 9.3.1 Key layout via `Storage` (SPEC §9.4): blob manifests at `blobs/<xx>/<id>` (`put_if_absent`), staged chunks at `blobs/uploads/<upload_id>/<chunk_idx>` (`put_if_absent`, never overwritten); reads check the manifest against the `BlobRef` (`check_ref`)
- [x] 9.3.2 Parallel chunk upload/download, resumable upload by chunk existence check (SPEC §9.4: the resume token is `upload_id` + `chunk_size`; a `Chunker` that starts at chunk k with a hasher fed the stored chunks; `AlreadyExists` on a chunk restarts under a new upload id)
- [x] 9.3.3 Small-blob single-object optimization (threshold)
- [x] 9.3.4 `put_blob` resumes a remote upload: `PutBlobOptions::resume: Option<ResumeToken>` drives `RemoteBlobStore::resume`, `Chunker::resume` and `restage`, pinning the old upload id before listing and the new one before `restage`, and rejecting a token chunk size below the floor (SPEC §9.4, §9.5; found in 9.3.2). Done as: `put_blob` resume in `crates/persia/src/internal/blob/resume.rs` over `RemoteBlobStore::resume`/`restage` (#110); audited claim by claim in the resume audit (#112), with token, pack-rejection, SimFs crash and GC orphan tests (`blob_resume_tokens.rs`, `blob_resume_crash.rs`)
- [x] 9.3.5 Seed the `blob_manifest` fuzz corpus with the `inline-*.blm` golden files (found in 9.3.3)
- [x] 9.3.6 (v1.0) A resumed upload refreshes its chunks' age, or `pin_contained` checks that chunk 0 exists: an upload resumed, older than the grace period and bound after the sweep's manifest listing could lose its chunks if its pins drop before the chunk phase (found in the 9.5 review). Done as: restage under a fresh upload id when nothing new is staged; the case of a resume that stages a new chunk is 9.5.10. Audited in the resume audit (#112): the restage is covered by the SimFs crash-during-resume test in `blob_resume_crash.rs`
- [x] 9.3.7 (v1.0) A resume whose reader ends exactly where the kept run ends silently binds a shorter blob when no `expected_id` is given; resume should require `expected_id` or the total size (SPEC §9.4, §9.6; found in the #112 review, pinned by test `a_reader_that_ends_where_the_kept_run_ends_binds_that_prefix_under_a_new_upload`) Done as: `PutBlobOptions::expected_size` (`BlobSizeMismatch`, binds nothing; it bounds the reader one byte past the size, and both checks run before the last window and the restage); a resume with neither is `ResumeUnchecked` before anything is read; the pinning test is now `a_reader_that_ends_where_the_kept_run_ends_binds_nothing`

## 9.4 Integration with documents
- [x] 9.4.1 `put(doc, blob)` ordering: blob durable → document commit (SPEC §9.5)
- [x] 9.4.2 Streaming API: `put_blob(reader)`, `get_blob_range(id, range)`
- [x] 9.4.3 Document `blob_ref` stored in `BlobRefs` section; included in hits
- [x] 9.4.4 Decide, before the blob manifest encoding is released (SPEC §15), whether `put_blob`'s chunk size has a floor or the blob manifest a size cap: `chunk_size` 1 lets a 4 GiB blob need a 16 GiB manifest (ADR-0035 Consequences)
- [x] 9.4.5 Wire `PackStore` into the database open: `<name>.persia.blobs` beside the container, created lazily by the first `put_blob` (SPEC §9.3), and `BlobDatabase` over `Database` plus the pack, passing the container's `db_uuid` (found in 9.4.1)
- [x] 9.4.6 [T] Crash-order test of `put(doc, blob)` through `SimFile` at every container write: needs a `persia` dev-dependency on `persia-storage` in `xtask` `ALLOWED` (found in 9.4.1)
- [x] 9.4.7 A store that refuses every write until reopened (the pack after a failed sync, `PackPoisoned`) ends `put_blob`'s bind retries at once instead of at the end of `bind_budget`: an `Error::is_store_poisoned` predicate in the contract (found in 9.4.1 review)

## 9.5 Garbage collection
- [x] 9.5.1 Mark (SPEC §9.5): referenced ids from all live manifests/snapshots and WAL records from `wal_start`, plus the writer's pin set (pinned blob ids and upload ids from `put_blob`), then the upload ids their blob manifests name
- [x] 9.5.2 Sweep (SPEC §9.5): unmarked blob manifests before their upload's staged chunks, older than the grace period, each pin check and delete under the pin set's lock; remote listing per `blobs/<xx>/` prefix and `blobs/uploads/`; pack-file rewrite for local
- [x] 9.5.3 `persia verify`: referenced blob missing → error; checksum scan
- [x] 9.5.4 [T] crash tests between blob write and doc commit; GC safety test (no live blob ever deleted)
- [x] 9.5.5 `ObjectMeta.last_modified` (SPEC §10.1 update) so the sweep (9.5.2) can apply the grace period to remote objects
- [x] 9.5.6 (v1.0) [T] No dangling blob reference (SPEC §9.5, §9.6): a document committed with a `BlobRef` that has no blob behind it (set below the public API with `Document::set_blob_ref`, or after copying only the container, §4.8) is reported by `verify` as an error naming the collection, document id and `blob_id`, in a segment and in the WAL; the public `put(doc, blob)` given a `BlobRef` its database never bound is rejected and logs nothing
- [x] 9.5.7 `SimFs` file modification times from an injectable clock, so sweep crash tests on `LocalStorage<SimFs>` can age objects (SPEC §10.1; found in 9.5.5)
- [x] 9.5.8 Sweep (9.5.2): an upload's staged chunks are not deleted while a pinned blob's manifest names it, checked under the pin set's lock (`PinSet::unless_pinned`, ADR-0051; found in the 9.4.1 review)
- [x] 9.5.9 (v1.0) [T] GC concurrent with `put_blob` and `get_blob` on other threads over a real `PinSet` (the 9.5 tests simulate pin races through a pin-gate hook, and the remote path at the `persia` level needs a clocked `MemoryStorage`: a `persia` dev-dependency on `persia-storage` in xtask `ALLOWED`, as 9.4.6 does, or a `persia-blob` `testing` re-export; found in 9.5.2). Done as: `crates/persia/tests/gc_concurrent.rs` (#110), GC rounds beside `put_blob`/`get_blob` threads over a real `PinSet` on a clocked `MemoryStorage` (via the existing `persia-storage` dependency) and on the pack
- [x] 9.5.10 (v1.0) The remote sweep re-lists an upload's chunks under the pin set's lock before deleting them, and keeps the upload when a chunk is younger than the grace period or was not in its first listing (SPEC §9.5, ADR-0053): a resume that pins, lists, stages a new chunk under the old upload, binds and drops its pins between the sweep's upload listing and its locked check, and is referenced later by `BlobRef`, can lose the old chunks; without `strong_list` the first listing can also miss a fresh chunk (found in the 9.3.6 review) Done as: under the pin set's lock the sweep lists `blobs/uploads/<id>/` again and deletes only when it equals the first listing (index, size, `last_modified`); any difference keeps the upload this round (`SweepReport::changed`), no retry. Tested by the interleavings in `crates/persia/tests/blob_resume.rs`, `gc_resume_interleaved.rs` and `crates/persia-blob/tests/gc_remote.rs`, `gc_remote_interleave.rs`; the case where both listings miss a chunk is 13.7.3

---

# M10 — Embedded API (`persia`)
Deps: M6, M7, M8, M9. First user-facing release: `v0.1`.

## 10.1 Public API design
- [x] 10.1.1 `Db::open(path|url)`, `OpenOptions`, `Db::close` Done as: `persia::Db::open` (a path or a hand-parsed `file://` URL; every other scheme `Unsupported`, SPEC §10.2), `OpenOptions` (SPEC §14 defaults), `Db::close` (final commit, writer lock released) and a bounded `Drop`; `tests/open.rs`, the `location` property tests and the `db_location` fuzz target (ADR-0063, ADR-0064).
- [x] 10.1.2 `Collection` handle: create/get/drop, schema inspection Done as: `persia::Collection` (an `Arc` of the database, no lock, `Closed` after close) and `Db::create_collection`/`create_collection_json`/`collection`/`collection_names`/`drop_collection`, `Collection::name`/`schema`; schema types re-exported (ADR-0063); `tests/collections.rs`.
- [x] 10.1.3 `put`, `put_batch`, `delete`, `get`, `commit`, `search`, `blob` APIs Done as: `Collection::put`, `put_batch`, `put_json`, `put_with_blob`, `delete`, `get`, `get_json`, `commit`; `Db::put_blob`, `get_blob` (`BlobReader`), `get_blob_range`; `Collection::search(&SearchRequest)` and `search_json` over the engine's `QueryRequest` (ADR-0062), cursors as opaque tokens; `tests/api_documents.rs`, `tests/search.rs`, `documents.rs` pin test (ADR-0063).
- [x] 10.1.4 Typed documents via `serde` (`put_struct`) + dynamic `Document` Done as: `Collection::put_struct` (exactly `put_json(serde_json::to_string(&T))`), `get_struct` and `to_struct` (on `Collection`: a `Document` needs the schema for names); SPEC §6.4.1 "Typed values"; `tests/typed.rs` with a round-trip property test.
- [x] 10.1.5 Error type, `Result` aliases, stable error codes Done as: `persia::Error` (opaque: `code`, `message`, `source`) and `ErrorCode` (the C ABI numbers and names, 13 `READ_ONLY`, 14 `POISONED`, 15 `CANCELLED` appended); one exhaustive mapping in `error/classify.rs`, corruption first; table tests (ADR-0063, `docs/sdk/c-abi.md`).
- [x] 10.1.6 Async-friendly wrapper (`spawn_blocking`) as optional feature `tokio` Done as: 10.1.11 (ADR-0064).
- [x] 10.1.7 The request's `"proximity"` (SPEC §7.1, §7.2): `search` plans `true` as `SearchPlan::with_proximity` (off by default since 7.3.5) and `false` as `SearchPlan::without_proximity`. The cursor already records whether its plan scored proximity and rejects the other setting (`RequestError::CursorProximity`, 7.3.5); the request maps that error. [T] A page after a cursor from a proximity-on search, asked with `"proximity": false`, is an error; with the same setting it pages exactly Done as: `QueryRequest::with_proximity` and the JSON key `"proximity"` plan the plain search with or without the bonus; `"proximity": true` on a request that relaxes is `QueryRequestError::ProximityRelaxed`, naming `"relax": false` (owner decision l, ADR-0062 item 10; across relaxed steps is 10.1.14). Tested in `crates/persia-engine/tests/request_proximity.rs` (`a_cursor_pages_only_under_its_own_proximity_setting` among them)
- [x] 10.1.8 (v1.0) `Db::backup(dest)`: a consistent copy of both files (SPEC §4.8), the container's committed state and then the pack holding every blob it references, with the writer open and without blocking it for the copy (no blob GC or compaction replaces a file meanwhile); a database without a pack copies one file. [T] A backup taken during writes and a GC opens with no dangling reference (`verify` passes), at the state of one commit Done as: `persia::Db::backup(dest)` (SPEC §4.8, ADR-0067): under the writer mutex a commit, a `CompactionHold` and a `GcHold`, then the container's and the pack's `CommittedImage` (header page checked, both slots holding the commit, frames to `log_end` read from a second handle); copied with the mutex released, written to temporary names, synced, hard-linked pack first and undone container first; never replaces a file; a read-only `Db` backs up the state it shows. Tested in `crates/persia/tests/backup.rs` (`a_backup_taken_during_writes_and_a_gc_is_one_commit_with_no_dangling_reference` among them), `crates/persia-storage/tests/container_image.rs` (crash sweeps of the writing and of the undo on `SimFs`) and `crates/persia-blob/tests/pack_image.rs`
- [x] 10.1.9 (v1.0) SQLite-like open, one rule for every language (SPEC §8.1, §13.1, §14): `Db::open(path)` creates the container and any missing parent directories when the path does not exist, and opens an existing container without replacing it (§4.5.1). `OpenOptions::create_if_missing(false)` (Go `WithCreate(false)`, TypeScript `{ create: false }`, C `PERSIA_OPEN_MUST_EXIST`) requires the container to exist and creates nothing. Read-only (`OpenOptions::read_only(true)`, `WithReadOnly`, `{ readOnly: true }`, `PERSIA_OPEN_READ_ONLY`) opens an existing container and does not create it or a parent. Read-only wins over `create_if_missing(true)` (Go `WithCreate(true)` with `WithReadOnly()`, TypeScript `{ create: true, readOnly: true }`, C `PERSIA_OPEN_READ_ONLY` alone): that combination is not an error, and a missing path fails. Today `persia_engine::db::Database` has separate `create` and `open` (`db.rs`), and `Container::create` fails when the parent directory is missing (`container/path.rs`). [T] A missing path whose parents are missing creates them and the file; `create_if_missing(false)` and read-only on a missing path fail; an existing file is opened and not replaced Done as: `Db::open` creates the missing parents durably through `persia_engine::db::create_parent_dirs` (SPEC §4.5.2; `db.rs` crash test on `SimFs`), then the container; `create_if_missing(false)` and read-only create nothing; racing opens retry with a bounded backoff; `tests/open_rule.rs`, `tests/db_process.rs` (racing creates across processes); SPEC §8.1, §13.1.
- [x] 10.1.10 (v1.0) Parse the request key `soft_filter` beside `filter` (same grammar and limits, SPEC §7.1.2) into `Compiled::with_soft_filter`; a filter given in both is hard (ROADMAP 8.2.13, ADR-0056). The request goes through `RelaxedQuery::from(&Compiled)`, never `From<&Query>` (which treats it as programmatic, whose marks may drop any root filter), so a request's `filter` stays hard Done as: `persia_engine::search::request`: `QueryRequest::from_json` reads every SPEC §7.1.4 key strictly (with `soft_filter` and `proximity`) and `QueryRequest::run` runs it on a `CollectionSnapshot`; `search_after` takes a versioned, checksummed base64url cursor token (`Cursor::to_token`, `from_token`) that records the sort's keys (ADR-0062, SPEC §7.4). Fuzz targets `request_json` and `cursor_token`; tested in `crates/persia-engine/tests/request.rs` (`filter_is_hard_and_soft_filter_is_what_last_resort_drops` among them) and `request_untrusted.rs`
- [x] 10.1.11 (v1.0) A runtime-agnostic async API we own (ADR-0064; replaces 10.1.6's `spawn_blocking` wrapper): `AsyncDb` / `AsyncCollection` behind the cargo feature `async`, which adds no dependency, with `open`, `close`, `put`, `put_batch`, `delete`, `get`, `commit`, `search`. Each `Db` gets its own pool of plain threads, started on first use (default `min(4, cores)`), joined on `close`, not shared with the maintenance thread. A future dropped before its job starts skips the job; a write that has started runs to the end and its result is discarded; a search or a get that has started has its `CancelToken` set; jobs still queued at `close` complete with `Closed`. [T] Each of those cases, driven by a `block_on` written in the test: no runtime is a dev-dependency (0.6.11). 10.1.6 is ticked "Done as: 10.1.11" only when this lands Done as: `persia::AsyncDb` and `AsyncCollection` behind the feature `async` (no dependency), every call a `JobFuture<T>` (a std `Future`) on a job queued to the database's own pool of `std::thread`s (`min(4, cores)`, started on first use, joined by `close`); `open` and `close` on threads of their own (ADR-0064 item 16). Tested in `crates/persia/tests/async_api.rs` (each cancellation case, driven by a `block_on` written in the test) and `crates/persia/tests/async_lifecycle.rs`
- [x] 10.1.12 (v1.0) A committed public-API snapshot of `persia` (ADR-0063 item 10): `crates/persia/public-api.txt`, a `just public-api` recipe that regenerates it, and a CI job that fails when the surface changes without the file. Check whether the snapshot expands re-exported engine items (`Document`, `CollectionSchema`, ...); if it does not, it covers them explicitly. `cargo semver-checks` waits for 10.5.1. [T] Adding a public item without regenerating the file fails the check Done as: `cargo xtask public-api [--check]` (`xtask/src/public_api.rs`) renders `persia`'s surface (default features and `async`) from rustdoc's JSON with the dated nightly, re-exported engine items expanded, into the committed `crates/persia/public-api.txt`; `just public-api`; CI's `repo-checks` runs `--check`. Fixture tests in xtask: adding a public item without regenerating fails the check. ADR-0063 amended (replaces Step 0 decision (a)'s `cargo public-api`).
- [x] 10.1.13 (v1.0) Response JSON (SPEC §7.5.1, §7.6, §7.7; ADR-0062): the JSON of a search response, written by the engine beside the request JSON (`persia_engine::search::request`). Hits: id, score, sort values, the stored fields after the projection, the blob reference, each hit's cursor as a cursor token (SPEC §7.4), and for a relaxed search its `level`, `step` and rank score. `relaxation` as SPEC §7.6 lists it (`level`, `applied`, `original`, `effective`, `corrected`, `truncated`, `fallback`, `filters_dropped`); a plain search (`relax: false`) states `level: "exact"` with nothing applied. `query_truncated`, the facets, and the explain tree of SPEC §7.7 with its `relaxed` node. Every score is a plain number (SPEC §7.2: no `Infinity`); decide the form of keyword values that are not UTF-8 (facet values, sort values). Wave 7 has typed results only (`QueryResponse`). [T] Golden responses; a response's cursor token pages the next request; every response is valid JSON (found in 10.1.10) Done as: `QueryResponse::to_json(&schema)` writes the response JSON of the new SPEC §7.5.2 (projected document, typed sort values, cursor token, level/step/rank score, facets, relaxation, `query_truncated`, explain tree), non-UTF-8 keyword values as `{"base64url": ...}`, `ResponseJsonError`; golden responses, a property, the request fuzz target; ADR-0065; the facade side is 10.1.25.
- [ ] 10.1.14 (v1.1) The proximity bonus across relaxed steps (SPEC §7.2, §7.6; ADR-0036, ADR-0062 item 10): today `"proximity": true` needs `"relax": false`, because the relaxed search plans every step without the bonus. Decide which steps score it (L0's words; an L2 rung's `should` clauses are not adjacent `must` words; L3 and L4 rewrite words into `prefix` and `fuzzy`, which are not words to the bonus), thread the setting through `search_relaxed` (its check, each step's plan, the hits' cursors), and gate the ranking change with nDCG@10 (SPEC §16.1). Then a request that relaxes takes `"proximity": true`. [T] A relaxed page under the bonus pages exactly by cursor, and a cursor from the other setting is `RequestError::CursorProximity` (found in 10.1.7)
- [x] 10.1.15 (v1.0) `PutBlobOptions`'s public fields name two `persia-blob` types that are not on ADR-0063's re-export list, `ChunkSize` and `ResumeToken` (found in 10.1.3): re-export them under `persia::` (an ADR-0063 amendment), or give the facade its own options type, so a caller can set `chunk_size` and `resume` without depending on `persia-blob`. [T] A program that depends only on `persia` sets both. Also here (found in the 10.1 review): make `PutBlobOptions` `#[non_exhaustive]` with a builder; not done in wave 7 because every test of the crate builds it with a struct literal Done as: the facade's own `#[non_exhaustive]` `PutBlobOptions` builder, `ResumeToken` and `Blob` (`crates/persia/src/blob/put.rs`), converted to `persia-blob`'s types inside the facade, and `Error::resume_token()`; ADR-0063 amended. `crates/persia/tests/blob_options_facade.rs` sets the chunk size and a resume with names from `persia` alone.
- [x] 10.1.16 (v1.0) `#[non_exhaustive]` and surface trim of the re-exported engine types before 10.5.1 (found in the 10.1 review; ADR-0063 Consequences): the schema types without it (`TextOptions`, `KeywordOptions`, `RankingConfig`, `RelaxationConfig`, `FieldKind`, `FieldType`, `Level`, `Fallback`, `FilterRelaxation`, `RankFn`, `StaticRank`), and the engine detail their methods expose through `persia` (`CollectionSchema::hash`/`hash_at`/`canonical_index_definition`, `from_json_in`/`AnalyzerRef::parse_in` naming `AnalyzerRegistry`, `Document::encode`/`decode`/`encoded_id`, `Document::extra`/`insert_extra` exposing `serde_json::Value`): hide, wrap or keep each, recorded in ADR-0063. [T] The public-API snapshot (10.1.12) lists none that was not kept on purpose Done as: the schema types are `#[non_exhaustive]` with one setter per field; `CollectionSchema::hash`/`hash_at`/`canonical_index_definition`/`from_json_in`, `AnalyzerRef::parse_in` and `Document::encode`/`decode`/`encoded_id` are `#[doc(hidden)]`; `Document::extra`/`insert_extra` kept (serde_json public). Each decision in ADR-0063's amendment table; the public-API snapshot test checks that none of the hidden items is listed. The rest is 10.1.29.
- [x] 10.1.20 (v1.0) A request JSON key for the projection (SPEC §7.5.1: every stored field, none, or a list of names). SPEC §7.1 names no key, so a JSON request returns every stored field and only a request built in code sets one (`QueryRequest::with_projection`). Decide the key and its forms with the response JSON (10.1.13), write them in SPEC §7.1.4 and parse them as strictly as the other keys. [T] Each form parses to its `Projection`, and a name that is not stored is an error (found in 10.1.10) Done as: the request JSON key `projection`, `"all"` (default), `"none"` or a non-empty array of distinct stored (or, outside `strict`, extra) field names, read strictly; ADR-0065 item 1, SPEC §7.1.4.
- [x] 10.1.21 (v1.0) Decide whether a request whose settings run L0 alone (`relax: false`) counts in the relaxation metrics (SPEC §7.6 "Metrics", ROADMAP 8.3.3). The request path runs the plain search for it (SPEC §7.1.4, ADR-0062 item 9), which counts nowhere: its zero-result queries are missing from `empty`, the zero-result rate. Running the relaxed search with the L0-only plan instead would count them and wrap explained hits in a `relaxed` node (SPEC §7.7), but cannot score the proximity bonus (10.1.14). [T] The chosen rule, read from `RelaxationStats` (found in 10.1.10) Done as: a request of L0 alone runs the plain search and counts its page from the top in `RelaxationStats` at level `exact` (`empty` when L0 found no hit); `search_with` without a request counts nowhere; ADR-0065 item 10, SPEC §7.6 Metrics.
- [ ] 10.1.22 (v1.1) Revisit the Windows default of mapped reads for read-only handles (SPEC §4.7; ADR-0017, amended 2026-11-23). Today `MappedReads::default()` is `Never` on Windows (`IfLocal` elsewhere), because Windows does not shrink a mapped file: a writer that stopped with a torn or corrupt tail cannot open read-write again while a mapped reader of the file is open, and each attempt on a corrupt tail may leave a quarantine copy (ADR-0014); and on a fixed drive formatted FAT32 or exFAT, which counts as local (ADR-0021), `MoveFileExW` may refuse to rename a compaction over a mapped file, so the writer's compactions fail without reclaiming space. Revisit it once the writer's open and compaction have a way through (truncate later, or retry with a clear error naming the readers), or map by default on NTFS only. [T] On Windows, a read-write open of a container with a torn tail, and a compaction on a FAT32 or exFAT drive, next to a reader opened with the new default (found in 10.2.4)
- [x] 10.1.23 (v1.0) The blob pack's read handle maps as the container's does (SPEC §4.7, §9.3; ADR-0049, ADR-0017 amended): `PackStore::enable_mapped_reads` takes a `MapPermit`, which only `unsafe MapPermit::new` makes, and `persia` forbids `unsafe`, so a read-only `Db` reads its pack with `pread` whatever its container does. Map a read-only pack on a local disk the way the container is mapped: an `unsafe` storage entry with the checks and contract of `Container::map_read_only_if_local` and the pack's writer lock, called from one audited module listed in `UNSAFE_ALLOWED` (ADR-0017 amended; no safe public function of `persia-storage` maps), use it in the read-only pack open by default, and let `OpenOptions` force `pread` for both files at once. [T] A read-only open on a local disk maps the pack and follows a pack rewrite; the read-write pack never maps by default (found in 10.2.4) Done as: `MapPermit::read_only_if_local` (unsafe, `persia-storage`), called from `persia-blob`'s one audited module `pack/store/read_only_mapped.rs` (in `UNSAFE_ALLOWED`), behind the safe `PackStore::open_mapped_if_local`, which the facade's read-only open uses with the container's `MappedReads`; a reopen after a rewrite maps the new file; `OpenOptions::mapped_reads(false)` forces `pread` for both files; Windows default `Never`. ADR-0017 amended; `crates/persia/tests/read_paths.rs`.
- [x] 10.1.24 (v1.0) A cursor records the rest of what its ranking depends on (SPEC §7.4: it records the sort, as a fingerprint of each key's field, order and missing placement, the proximity setting and the relaxation settings, but "does not record the query, its fields and filters, or the limits"): a token given back with another `q`, `fields`, `filter`, `soft_filter` or limits pages a different ranking without an error today. Record a hash of them in the cursor (a new cursor token version, ADR-0062 item 11) and refuse a page whose request differs, as a token of another sort, proximity setting or relaxation settings is refused. [T] A token of one query given to another is an error; the same request pages exactly (found in 10.1.7) Done as: cursor token version 2 records a request fingerprint (XXH3 of a canonical encoding of `q`, fields, filters, limits; `Cursor::request`, `QueryRequest::fingerprint`) and refuses another request (`RequestError::CursorRequest`); version 1 refused; ADR-0062 item 11 amended, SPEC §7.4.
- [x] 10.1.25 (v1.0) The facade writes the response JSON (SPEC §7.5.2, ADR-0065): `persia::SearchResponse` is a converted value that drops each hit's step, rank score and sort values, so it cannot write the JSON the engine writes (`QueryResponse::to_json`), and `search_json` returns typed values only. Give the facade a way to get the response JSON (a `to_json` that keeps what it needs, or a `search_json` form that returns text), which the C ABI (ADR-0018) and the server (M11) will pass through. [T] The facade's JSON is the engine's for the same request, golden-compared (found in 10.1.13) Done as: `SearchResponse` is the one source of the facade's response JSON: `impl Serialize` (`crates/persia/src/search/json.rs`) and `SearchResponse::to_json()`, no JSON built from strings; typed getters `Relaxation::corrected`/`fallback`/`filters_dropped`. Differential against the engine's `QueryResponse::to_json` (a table of requests plus 64 seeded cases), byte for byte; the engine holds the goldens. ADR-0065 amended; 10.1.30 and 10.1.31 follow.
- [x] 10.1.26 (v1.0) The rest of the embedded API as async (ADR-0064 item 16): 10.1.11 offers the eight calls of ADR-0064 item 11, plus `AsyncDb::open_with` and a synchronous `AsyncDb::collection`. Collection management (`create_collection`, `create_collection_json`, `collection_names`, `drop_collection`), blobs (`put_blob`, `get_blob`, `get_blob_range`, `put_with_blob`), `refresh`, `backup` and the JSON and typed variants (`put_json`, `get_json`, `put_struct`, `get_struct`, `search_json`) have no async form, so an async program creates its collections through a `Db` first. Decide which get one (a blob upload streams from a caller's reader, which a job on another thread cannot borrow) and add them on the same pool. [T] Each new call through a test `block_on`, and its cancellation case (found in 10.1.11) Done as: on the pool of ADR-0064 item 11, `AsyncDb::create_collection`, `create_collection_json`, `drop_collection`, `put_blob`, `get_blob_range`, `refresh`, `backup` and `AsyncCollection::put_with_blob`, `put_json`, `get_json`, `put_struct`, `get_struct`, `search_json`; uploads take an owned `R: Read + Send + 'static' reader (`put_with_blob` a reader plus `PutBlobOptions`, since `Blob<'r>` is not `Send`); `put_struct` writes its JSON on the caller's thread; `get_blob` not offered (its `BlobReader` blocks the caller, read by ranges instead); `collection_names` and the no-I/O calls (`AsyncDb::path`, `is_read_only`, `block_cache_stats`, `AsyncCollection::name`, `schema`, `durability`, `with_durability`, `to_struct`) synchronous on the async handles. A future dropped before its job starts skips it; a started change (uploads, backup, refresh, collection management, puts) runs to its end; a started read has its token set. `backup` checks and resolves `dest` at the call. Each call, with its reason, in ADR-0064's 2026-11-27 amendment (no Go, TypeScript or C forms: those bindings expose no async). Tests: `crates/persia/tests/async_calls.rs` (each call through the test `block_on`; skipped, started and closed-queue cancellation, no file or reader left, close then a clean reopen; a failing reader; a read-only database), `async_api.rs`.
- [x] 10.1.27 (v1.0) A public option for the async pool's size (ADR-0064 item 12: "`min(4, cores)` by default"): 10.1.11 has it only as the hidden test hook `OpenOptions::async_threads`. Make it a documented `OpenOptions` setter (and its Go, TypeScript and C forms when those bindings expose async), with 0 refused. [T] A pool of the size set starts on first use (found in 10.1.11) Done as: `OpenOptions::async_threads(usize)` (feature `async`), documented with a doctest; 0 is `InvalidArgument` at open (`Db` and `AsyncDb`, nothing created); no upper bound; default `min(4, cores)`. The pool starts on the first job, all or nothing: a thread the OS refuses fails that job with `Io` (the `io::Error` as `source()`), the threads already started are joined, and the next job retries the start; no preallocation by the count. SPEC §14 and ADR-0064's 2026-11-27 amendment (no Go, TypeScript or C forms yet: those bindings expose no async). Tests: `async_api.rs` (sizes 1, 3 and the default start on first use and not before; 0 refused at open; a refused thread; `usize::MAX` fails its first job without a panic), the `async_lifecycle.rs` thread census, a pool unit test.
- [ ] 10.1.28 (v1.1) `Db::backup` to a file system without hard links (ADR-0067 item 4): the backup links its files to their names so that it never replaces one, as `Container::create` does, so a FAT32 or exFAT drive (a common backup target) refuses it with `Io`. Decide a way that keeps "never replaces, never a container without its pack" there (an exclusive create of the final name written in place, then a marker; or a backup directory renamed into place), and record it in SPEC §4.8. [T] A backup on a file system whose `hard_link` is unsupported (a `Vfs` that refuses it) succeeds and still never leaves `dest` without its pack at any crash point (found in 10.1.8)
- [x] 10.1.29 (v1.0) The rest of the engine detail the public-API snapshot (`crates/persia/public-api.txt`, 10.1.12) shows through the re-exported types, beyond 10.1.16's list (found in 10.1.12): `BlobRef::encode`, `decode` and `encoded_len` name `persia_format::reader::Reader`, `writer::Writer` and `error::Result`, and `BlobRef::new` returns `persia_format::error::EncodeError`; `Filter::from_json` and `Filter::validate` take `persia_engine::query::limits::QueryLimits`, which `persia` does not export. Hide, wrap or keep each, recorded in ADR-0063. [T] The snapshot lists none that was not kept on purpose Done as: `BlobRef::new` hidden and wrapped by `persia::blob_ref` (`InvalidArgument`, the format error as `source()`); `BlobRef::encode`/`decode`/`encoded_len`/`VERSION`/`MAX_ENCODED_LEN` and `Filter::from_json`/`validate` `#[doc(hidden)]`; the engine errors of ADR-0063 item 4 and `ParseBlobIdError` kept. Each decision in ADR-0063's 2026-11-27 amendment, SPEC §9.6 names `blob_ref`; the snapshot test checks no hidden item, `persia_format::` or `QueryLimits` is listed; `public-api.txt` regenerated.
- [x] 10.1.30 (v1.0) Typed access to what a hit of `persia::SearchResponse` keeps for its JSON (found in the 10.1.25 review): its sort values typed by their keys (`TypedSortValue`, or a facade type), and for a relaxed search its step and rank score, as public, documented getters of `Hit`. [T] The getters state what `SearchResponse::to_json` writes for the same hit Done as: `Hit::sort_values() -> &[persia::SortValue]` (a `#[non_exhaustive]` facade enum: `Score`, `I64`, `F64`, `Datetime` in microseconds, `Bool`, `Keyword`, `Missing`), `Hit::step()` and `Hit::rank_score()`, each doc naming what `to_json` writes; the engine's `TypedSortValue::Datetime` holds `i64` (breaking, CHANGELOG), and a stored sort key no document can hold is `Corrupt` at the search (SPEC §5.2.1 amended). ADR-0065 amended; `crates/persia/tests/hit_getters.rs` (a table of 11 requests and 64 seeded cases against the parsed JSON), `crates/persia-engine/tests/sort_corruption.rs`.
- [x] 10.1.31 (v1.0) One writer of a document's JSON form shared by the engine and the facade (found in the 10.1.25 review): the facade's response JSON renders each hit's document with `Document::to_json` and parses it back into a value (`search/json.rs`, `DocumentJson`), so every document is serialized twice. Give the engine a `Serialize` view of a document's JSON form (SPEC §6.4.1, key order `id`, `blob`, fields by name) that both `Document::to_json` and the facade use. [T] The facade's JSON is still byte-for-byte the engine's (the differential tests of 10.1.25); a bench of `to_json` on a page of large documents Done as: `persia_engine::document::DocumentJson`, a checked `Serialize` view (`id`, `blob`, fields by name; the blob key order pinned in SPEC §6.4.1), is `Document::to_json` and is serialized in place by the facade's `search/json.rs`; the 10.1.25 differential tests unchanged and byte for byte, plus a proptest against a test-only copy of the previous writer. Bench `crates/persia/benches/response_json.rs` (20 hits of 64 KiB): `SearchResponse::to_json` 3.77 ms → 1.06 ms, `Document::to_json` 83.5 µs → 50.9 µs on one machine. ADR-0065 amended.
- [ ] 10.1.32 (v1.1) On Windows, a read-only `Db` opened with `mapped_reads(true)` maps its blob pack too (10.1.23), and a mapped pack has the container's Windows consequences for the pack's writer (found in the 10.1.23 review): a blob GC that rewrites the pack may fail to rename the new file over it on FAT32 or exFAT, and the writer's next open cannot truncate a torn pack tail while the mapping lives. Revisit with 10.1.22: name the readers in the error, or retry. [T] On Windows, a GC rewrite and a torn-tail open of the pack next to a reader that maps it
- [x] 10.1.33 (v1.0) A caller's `BlobRef` that does not match its blob is not damage (found in the 10.4.x batch review): `put` of a document whose reference has the right `blob_id` but another `size` or `chunk_size` fails in `check_ref` with `persia_blob::Error::Corrupt`, so the embedded API reports `ErrorCode::Corrupt` and `persia import` exits with the corruption status 3 for bad input. SPEC §9.6 does not decide how such a reference is reported. Decide it (a typed variant, e.g. `InvalidArgument` for a reference that names a bound blob with other attributes), record it in SPEC §9.6, and make the CLI read the facade's codes: `persia import` and `persia search` exit 3 for `ErrorCode::Corrupt` (stored damage reached through a put or a read, such as a damaged blob manifest frame) and 1 for a mismatched caller reference. [T] A wrong-`size` or wrong-`chunk_size` reference line exits 1, names its line and logs nothing; a damaged manifest frame reached through a put exits 3 Done as: a caller's mismatched reference is the typed `persia_blob::Error::BlobRefMismatch { blob_id, field, given, stored }` (`BlobRefField`), reported by the new `BlobManifest::check_given_ref` and mapped to `ErrorCode::InvalidArgument` (SPEC §9.6): a reference the caller passes is input, on a put (`put`, `put_batch`, `put_with_blob`, `BlobStore::contains`; nothing logged) and on a read (`Db::get_blob`, `get_blob_range`, `read_range`; the same `blob_id` can hold another chunk size in another database). Decided by call site, never by text; a stored reference that disagrees with its manifest is still `Corrupt` (`check_ref`, used by `verify` only), and a manifest naming another blob is `Corrupt` everywhere. SPEC §9.4 amended, ADR-0034/0035 notes. `persia import` and `persia search` read `persia::Error::code()` first: 3 for `Corrupt`, 1 for the mismatch (a segment tombstone past its docids is now 3 there too). Tests: `persia-cli/tests/import.rs` (exit 1 for wrong size and chunk size, the line named, nothing logged; a damaged manifest frame reached through a put exits 3, synchronised by the pipe, no timer), `persia/tests/api_documents.rs`, `documents.rs`, `dangling.rs`, `persia-blob/tests/manifest.rs`, store and classify unit tests.
- [x] 10.1.34 (v1.0) The website guides still parse a filter with `Filter::from_json` (found in 10.1.29, #136 open question 1): `website/src/content/docs/guide/filtered-search.md` and `typo-tolerant-search.md` tell users to call it, but 10.1.29 made it `#[doc(hidden)]`, and the pages were already stale ("no search on `persia::Db`"). Rewrite them against the current facade (and 10.1.35 once it lands). [T] Each code sample of the two guides compiles against the public API (a doctest or a checked snippet) Done as: `filtered-search.md` and `typo-tolerant-search.md` rewritten against the facade (`Collection::search` / `search_json`, `SearchRequest::filter_json` or a typed `Filter`, hard and soft filters, L6 dropping only the `soft_filter`); each Rust sample is a whole visible `fn main` program (Starlight shows `# ` lines literally), compiled and run as a doctest of `crates/persia/src/guides.rs` (`#[cfg(doctest)]`, `#[doc = include_str!(..)]` per page) by the existing `cargo test --workspace`, no new tool; `cargo xtask` repo check `guide_samples_are_doctests_of_the_public_api` fails when `mod guides` is unwired, a listed page is missing or has no Rust sample, or a Rust fence names `persia_engine`, `persia::internal`, `QueryLimits` or `Filter::from_json` (rustdoc alone would compile those, as doctests see every dependency); the L6 filter text corrected on `faceted-search.md`, `fuzzy-search.md`, `getting-started.md`, `features.md`; the other stale pages are 10.1.41
- [x] 10.1.35 (v1.0) A facade way to parse a filter from JSON (found in 10.1.29, #136 open question 2): with `Filter::from_json` hidden (it takes the engine's `QueryLimits`), a caller can only send a filter inside a whole request JSON. Decide a public form (e.g. a `SearchRequest` filter-JSON setter, or a facade `Filter` parser under the database's limits), record it in SPEC §7 and ADR-0063, and add it to `public-api.txt`. [T] A filter parsed alone and the same filter inside a request JSON give the same hits; a filter past the limits is `InvalidArgument` Done as: `SearchRequest::filter_json` and `soft_filter_json` (SPEC §7.1.4, ADR-0063 amendment of 2026-11-27, with the reason: a `SearchRequest` holds no schema and already resolves names at run time, so no new method on `Collection`/`AsyncCollection`, the two forms cannot drift and cursors are interchangeable); the text is parsed at run time by the engine's `Filter::from_json` (fuzzed by `filter_json`) under the snapshot's schema and the same limits as `search_json` (one private `query_limits()`; `QueryLimits` stays unexported); the later of `filter`/`filter_json` wins. Malformed or invalid filter text is `InvalidArgument`; a filter past `max_terms`, `max_clauses` or `max_depth` is `Limit`, not `InvalidArgument` as the [T] above says, because ADR-0063 item 8 makes every limit `LIMIT` and the same filter inside a request JSON is already `Limit` (decided 2026-11-27, recorded in the amendment). Tests `crates/persia/tests/filter_json.rs`: a seeded proptest comparing the response JSON (or error code) of the setter form and the request-JSON form for hard and soft filters with relaxation on and off, unit cases, one test per limit, refused text by `ErrorCode` against both forms; doctests on both setters; `public-api.txt` +2 lines
- [x] 10.1.36 (v1.0) `JobFuture`'s auto-trait impls name a private type (found in 10.1.11, #136 open question 3): `crates/persia/public-api.txt` lists them `where Arc<Slot<T>>: ...`, naming the facade's private `Slot<T>`. Make the bounds read in public terms (e.g. state `Send`/`Sync`/`Unpin` for `T: Send` explicitly, or a public-API renderer rule), and record it in ADR-0064. [T] The snapshot names no private facade type; a compile test of `JobFuture<T>: Send` for `T: Send` Done as: today's conditions kept and written in the ADR-0064 amendment of 2026-11-27 (`Send` and `Sync` exactly when `T: Send`, `Unpin` always); `Unpin` is an explicit safe impl; `Send`/`Sync` need `unsafe` to implement by hand and rustdoc renders auto-trait bounds as unreduced field types, so a renderer rule: `STATED_AUTO_TRAIT_BOUNDS` in `xtask/src/public_api/render.rs` states them, rendering fails when a synthesized bound names a private type with no entry or an entry goes unused; `public-api.txt` now reads `where T: core::marker::Send` (3 lines replaced, no private name); `crates/persia/tests/job_future_auto_traits.rs`: generic compile-time checks plus self-checked const auto-trait probes (`JobFuture<Rc<u8>>` and `JobFuture<MutexGuard<u8>>` neither `Send` nor `Sync`, `JobFuture<Cell<u8>>` both); no `unsafe`
- [ ] 10.1.37 (v1.1) Cooperative cancel of a started async upload or backup (found in 10.1.26, #138 open question 1): once started, `put_blob`, `put_with_blob` and `backup` run to their end when their `JobFuture` is dropped; a cancel would need a cancellation point in `write_backup` and in the blob writer, and a cancelled upload or backup must leave nothing behind. [T] A started upload and a started backup whose future is dropped stop at their next cancellation point and leave no blob, pack entry or backup file
- [ ] 10.1.38 (v1.0) A read-only `refresh` holds the write side of the state lock during the pack refresh (found in 10.1.26, #138 open question 2): on a read-only database the synchronous lookups (`collection`, `collection_names`, `schema`, `to_struct`) wait while a running `refresh` refreshes the blob pack. Move the pack refresh out from under the lock, or record why it must stay. [T] A lookup completes while a refresh is held inside its pack refresh (a test hook, no sleep)
- [ ] 10.1.39 (v1.1) Cache a refused async pool start until the size changes (found in 10.1.27, #138 open question 3): a refused start is retried in full on every later job, under the pool lock. Decide whether to keep the refusal until `async_threads` changes (or the database reopens), and record it in ADR-0064. [T] After a refused start, later jobs fail without a new start attempt (counted through the refusal hook)
- [x] 10.1.40 (v1.0) `cargo clippy -p persia --lib` without `--all-features` fails (found in #138, open question 5): two unused imports in `crates/persia/src/internal/gc.rs:29-30` under the default features; CI runs only `--all-features`, so it does not show. Fix the imports and decide whether CI also lints the default feature set. [T] `cargo clippy -p persia --lib -- -D warnings` passes Done as: the CLI-only re-exports `BlobError` and `Rewrite` in `internal/gc.rs` gated on `#[cfg(feature = "internal")]`; the CI lint job and `just lint` also run `cargo clippy -p persia --lib -- -D warnings`, `--features async` and `--features internal` (the sets a user and persia-cli/fuzz build; `--all-features` turns `internal` on, which hides an item only the CLI reaches; `slow` gates only tests and `test-hooks` implies `internal`, so a full powerset adds nothing; `--lib` only, as the tests' dev-dependency on persia adds `test-hooks`); an unused import re-added under default features fails the new lines and passes the `--all-features` one
- [x] 10.1.41 (v1.0) The other website guides name engine, internal or removed items (found in 10.1.34): under `website/src/content/docs/guide/`, `architecture.mdx`, `blobs.md` (sample uses `persia::internal::documents::BlobDatabase` and `persia_engine`), `code.md`, `comparison.md`, `durability.md` (engine `Database` as the API), `embedded-api.md` (the Db/Collection API called planned/illustrative; :33 still says L6 drops the request's filters), `embedded-search.md`, `faceted-search.md` (sample at :115 uses `Database::create_on`, `SimFile`, `CollectionViews::search`), `faq.md`, `features.md`, `file-format.mdx`, `for-agents.md` (`persia_engine::db::Relaxation`, `QueryCompiler`; :47 the old L6 filter step), `fuzzy-search.md` (`RelaxPlan`, `QueryLimits`, an ILLUSTRATIVE sample), `ranking.md`, `sdks.md`, `synonym-and-prefix-search.md`, `text-analysis.md` (`QueryCompiler`, `QueryLimits`), and the `website/src/status.mjs` tokens (`%%MATCHING_CELL%%`, `%%QUERY_CELL%%`, `%%RELAXATION_CELL%%`, `%%ENGINE_CALLS%%`) still say `persia` exports no `Db`, call `Db::open`/`search` illustrative, or show the engine's `CollectionViews::search`. Rewrite them against the facade and add their Rust samples to `crates/persia/src/guides.rs`. [T] Every Rust sample of the guides is a doctest of the public API, and no page names an item `public-api.txt` lacks Done as: every listed page and the `status.mjs` tokens rewritten against the facade (`status.mjs` reads the facade's calls from `public-api.txt` and names a `SearchRequest` capability only when its setter is there); the Rust samples are whole visible `fn main` programs, run as doctests of `crates/persia-docs` (10.1.42 moved them out of `crates/persia/src/guides.rs`), which lists 12 pages (`getting-started.md` added); new repo check `guides_name_only_the_public_api` (`xtask/src/repo_checks/guide_api.rs`, ADR-0063 amendment item 5) reads every `persia::` path (use groups expanded, a leading `::` and blanks around `::` read too) and every path starting with a persia type name, in fences and inline code (backtick spans of any length, `<code>`), on the guide pages and the docs root pages, and requires it in `public-api.txt`, refuses the lower crate names anywhere and an `ENGINE_ONLY` list of engine types as words of code, and requires a block the site shows as Rust to run on a listed page; engine internals described on purpose sit in marked parts (`<!-- engine-internals -->` in .md, `{/* engine-internals */}` in .mdx, first line saying "internal"), which skip only the engine-word and type-name rules: an explicit `persia::` path is still checked, only `persia::internal::*` exempt (`architecture.mdx` names `persia::internal::documents` there as an internal); renamed imports and method calls on values are documented limits, refused in samples by the doctests; every inbound `#anchor` kept; SPEC §7.2 notes the static rank scores only L5 hits today (10.1.45); `public-api.txt` unchanged; the gaps found are 10.1.44–10.1.48; the landing page's sample is 10.1.51
- [x] 10.1.42 (v1.0) The guide doctests read files outside the `persia` crate (found in the 10.1.34 review): `crates/persia/src/guides.rs` uses `include_str!("../../../website/...")`, which `cargo package` cannot ship, so the first crates.io publish of `persia` would fail. Move the check (e.g. a doctest crate that is never published, or an xtask step) before any `publish = false` is flipped. [T] No `include_str!`/`include_bytes!` in `persia`'s sources reaches outside `crates/persia/` (a repo check), and the guide samples still run in CI Done as: new crate `crates/persia-docs` (`publish = false` in its own manifest; `ALLOWED`, the no-async `EMBEDDED` list and CLAUDE.md updated; ADR-0063 amendment items 1–3): its lib is the `#[cfg(doctest)]` guides module, its dev-dependencies `persia` (default + `async`) and `tempfile`; persia's C ABI table test, a second include reaching outside the crate, moved there too (against the public `ErrorCode::ALL`); `cargo test --workspace` runs the doctests, but cargo unifies persia-cli's `internal` into persia there, so CI's test job and `just test` also run `cargo test -p persia-docs`, where a sample naming `persia::internal` fails to compile (hidden items still compile, so the name check stays); repo check `persia_includes_nothing_outside_its_directory` resolves every `include_str!`/`include_bytes!`/`include!` (any delimiter; another one reported) and `#[path = "…"]` in `crates/persia/` against its file and refuses a path leaving the crate, an absolute or a non-literal one
- [x] 10.1.43 (v1.0) The guide-sample repo check reads a fence as Rust only when it is untagged or its first tag is `rust` (found in the 10.1.34 re-review): `guide_samples_are_doctests_of_the_public_api` (`xtask/src/repo_checks.rs`) skips fences rustdoc still compiles, such as `no_run`, `ignore`, `should_panic`, `compile_fail`, `edition20xx`, so a `no_run` sample naming `persia_engine` passes; it also finds pages by splitting on `include_str!("`, which a comment could match. Classify fences as rustdoc does, and match only `#[doc = include_str!("`. [T] Unit tests of the fence classifier (`no_run`, `rust,ignore`, `title="…"`) and of a commented `include_str!` Done as: `doctest(info)` in `xtask/src/repo_checks/guides.rs` ports rustdoc 1.99's info-string tokenizer and Rust rule (ADR-0063 amendment item 4) and says whether a block runs; unit-tested by a table of 67 info strings with what this toolchain's rustdoc did with each (`rustdoc --test --test-args --list`): `rust title="src/main.rs"` is no doctest at all (the tokenizer stops at `=`), order matters (`json,no_run` is not Rust, `no_run,json` is), `edition20xx` sets neither flag, `{.rust}` is Rust, `json`/`text`/`toml` are not; pages are found only from `#[doc = include_str!("…")]` outside `//`, `///` and nested `/* */` comments
- [x] 10.1.44 (v1.0) A deadline on `SearchRequest` (found in 10.1.41): SPEC §7.3 lets a search carry a deadline (an instant of an injected clock) and the engine reads one, but the facade's `SearchRequest` has no setter and its request JSON no key, so only an async search's dropped future stops a running query. Decide the public form (a `deadline`/`timeout` setter, its JSON key, the clock it reads) and its `ErrorCode` (`Cancelled`), record it in SPEC §7.1 and ADR-0063. [T] A search past its deadline fails with the decided code at the engine's next check (an injected clock, no sleep); the guides' faceted-search "Deadlines" section names the call Done as: `SearchRequest::timeout(Duration)` and the request-JSON key `timeout_ms` (a whole number of milliseconds, 0 to `u64::MAX`; a fraction, an exponent, `-0`, `null`, a string or 2^64 is `InvalidArgument`, unknown keys and misspellings stay refused; 0 has passed at the first check; an end the clock cannot hold is no limit), a timeout rather than an instant because an `Instant` means something only on the clock that made it; read on the database's clock (`OpenOptions::clock` under `test-hooks`, the system's otherwise), which the facade now hands every search as the relaxation budget's clock too, from when the request starts to run (after `search_json`'s and `filter_json`'s parse; an async search's from when its job starts on the pool); expiry is `ErrorCode::Cancelled` (15), whose doc already covers a deadline, so the C ABI table and `ErrorCode::ALL` are unchanged; the timeout is an error at every relaxation step, `budget_ms` stays the soft limit; recorded in SPEC §7.1, §7.1.4, §7.3, §7.6 and an ADR-0063 amendment; the faceted-search "Deadlines" section names the call in a persia-docs doctest; tests with a clock that ticks per read or never moves, no sleep; the `request_json` fuzz target checks what a parsed `timeout_ms` does, with seeds at 0 and `u64::MAX`; the open questions are 10.1.61 and 10.1.62
- [x] 10.1.45 (v1.0) A query hit's score does not include the collection's static rank (found in 10.1.41): SPEC §7.2 writes the total as "Σ_t score + static_rank contribution (§6.2.3)", but the engine applies `score::static_rank` only to L5 fallback hits (`db/relaxed/fallback.rs`), so `ranking.static_rank` never changes a query's order. Decide which is right, and make SPEC §7.2 and the scorer agree (with an explain node for the contribution if it is added). [T] A query over two documents of equal text score ranks the one with the higher static rank first, or SPEC §7.2 says it does not; `ranking.md`'s "Static rank" section states the decision Done as: the scorer changed, SPEC §7.2 stands: every query hit's score is `sat(text + weight · fn(v))` (a wrapper at the plan's root, `search/static_rank.rs`), so `ranking.static_rank` lifts equal text matches in every ranking, L5 scoring its hits by the contribution alone (no double count); pruning stays exact with a view bound `M`, the contribution of the segment's largest key, read once per immutable segment and cached (a corrupt block is an error every time, never cached; a column with no values bounds 0; the memtable takes no threshold; an exhaustive search reads no bound); explain node `Explanation::WithStaticRank` (SPEC §7.5.2, §7.7, both JSON writers); the reference model and the planning proptest cover i64, f64 and datetime rank fields; relax_quality, relax_zero_results and relevance read the same before and after (no corpus there sets a static rank); the engine's search benches show collections without a static rank unchanged and ranked ones paying about 2x on a term and +27% on a pruned 3-term OR (`docs/benchmarks.md`); ADR-0044 amendment, `ranking.md`'s "Static rank" section and CHANGELOG state it; the open questions are 10.1.63 to 10.1.67
- [ ] 10.1.46 (v1.1) Registered analyzers through the embedded API (found in 10.1.41): SPEC §6.2.2 lets a schema name an analyzer registered with the engine's `AnalyzerRegistry` (ROADMAP 6.6.8), and `persia-cli` passes one through `persia::internal`, but `OpenOptions` takes no registry, so a `persia` program can only name built-in analyzers. Decide a public form (an `OpenOptions` registry setter and a facade way to build an analyzer, or a documented limitation), record it in ADR-0063. [T] A collection whose text field names a registered analyzer is created, put and searched through `Db`; opening it without the registry is the SPEC's error
- [ ] 10.1.47 (v1.1) Blob garbage collection from the embedded API (found in 10.1.41): GC runs only as `persia gc`, which opens the database read-write, so a long-running application must close its `Db` to collect. Decide whether `Db` offers it (a `gc` call with the grace option and a report type in facade terms, ADR-0052) or record why not. [T] A collection run through `Db` while reads and `put_blob` run on other threads deletes only unreferenced blobs (the GC differential cases)
- [ ] 10.1.48 (v1.1) Opening past a corrupt tail that may hold acknowledged writes (found in 10.1.41): SPEC §4.4 says such an open fails "unless the caller asks for quarantine", and the storage layer has `Repair::Quarantine`, but neither `OpenOptions` nor the `persia` CLI can ask for it, so a database in that state cannot be opened through the public API. Decide the operator path (an `OpenOptions` setter, or a `persia` command that opens with quarantine and names the quarantine file), record it in SPEC §4.4. [T] A container whose tail holds a frame of a newer `frame_seq` after a damaged frame fails `Db::open` with `Corrupt`, then opens through the decided path with the bytes kept in a quarantine file
- [x] 10.1.49 (v1.0) The `public-api` rustdoc build shows 7 `persia_engine` rustdoc warnings (found in #139, open question 2): they predate 10.1.29–10.1.36 and nothing fails on them, so new ones can join unnoticed. Fix them and make the build fail on a rustdoc warning (e.g. `RUSTDOCFLAGS="-D warnings"` in `cargo xtask public-api`, or a `cargo doc` line in an existing CI job). [T] `cargo xtask public-api --check` (or the chosen line) passes with no rustdoc warning and fails when one is added Done as: six links to private items became code spans, and `CollectionSchema::relaxation` resolves through its full path. `cargo xtask public-api` adds `-D warnings` to `RUSTDOCFLAGS`, so the existing repo-checks step `public-api --check` fails on a new warning. That rustdoc run already covers persia and the crates it re-exports from (engine, blob, format, analysis), with no second docs build. The ungated crates are listed in ADR-0063's snapshot amendment item 7. A broken intra-doc link fails the step
- [ ] 10.1.50 (v1.0) `Db::backup` with a relative `dest` is untested (found in #139, open question 3; #138 open question 4): the tests back up only to absolute paths, so how a relative `dest` (and its `<dest>.blobs` beside it) resolves is unchecked. [T] A backup to a relative `dest` writes the copy and its `.blobs` where SPEC and the docs say, and the copy opens
- [x] 10.1.51 (v1.0) The landing page's Rust sample is not a doctest (found in the 10.1.41 review): `website/src/content/docs/index.mdx` shows a `rust title="src/main.rs"` block, which rustdoc does not collect (its tokenizer stops at `=`), so its calls and its assert are never run; the name check scans it, the sample rule covers only guide pages. Make it one (a title form rustdoc accepts or a tempdir sample listed in `crates/persia-docs`), or show it as a checked excerpt of a guide sample. [T] Breaking the landing sample fails CI Done as: `index.mdx` is unchanged, and the hero looks the same. Its twin is `crates/persia-docs/src/samples/landing.md`, a doctest of the new `#[cfg(doctest)] mod samples`. It is the same program plus one hidden line that makes the temp dir. `shown_samples_equal_a_doctest` (10.3.3's check) requires the hero and its `toml` block to equal the twin as rustdoc shows it. The check runs in ci.yml's test job, which has no paths filter. ADR-0063 guides amendment item 8. A broken hero fails the xtask check, and the same break in both files fails the doctest Amended (#141 review): the hero now shows the README quickstart's program and `toml` block, and its twin is the getting-started guide's first sample, the README's too, so the landing page, the README and that guide are one program; `crates/persia-docs/src/samples.rs`, `samples/landing.md` and the `mod samples` wiring are removed, and the hero's `main` now returns `Result<(), Box<dyn std::error::Error>>` and ends with `db.close()?; Ok(())`.
- [x] 10.1.52 (v1.0) The website's node tests are not run in CI (found in 10.1.41): `website/src/status.test.mjs` (which holds the rule that no status token names an engine item or a capability the facade lacks, ADR-0063 amendment item 5) and `layers.test.mjs` run nowhere, and `layers.test.mjs` has 2 failing tests on main. Fix the failures and add a step to `website.yml`. [T] The website workflow runs both files and fails when a token names `CollectionViews` Done as: the tests were stale, not the code. The two live-snapshot tests hard-coded an old PROGRESS.md (engine and blob "in progress", the API "stub", M6 in progress). They now derive their expectations from the parsed milestone states and counts. website.yml's build job gained a `Status and layer tests` step (`node --test website/src/status.test.mjs website/src/layers.test.mjs`) on the job's `.nvmrc` Node, before `npm ci`. `crates/persia/public-api.txt` joined both paths filters, since status.mjs reads it (ADR-0063 snapshot amendment item 5). A token naming `CollectionViews` fails the step's command Amended (#141 review): both paths filters now cover every file outside `website/` that `status.mjs`, `progress.mjs` and the node tests read (`Cargo.toml`, `ROADMAP.md`, `PROGRESS.md`, `crates/persia/public-api.txt`, `crates/persia/src/lib.rs`, `crates/persia/src/internal/blob.rs`, `crates/persia-engine/src/**`, `crates/persia-blob/src/lib.rs`, `crates/persia-analysis/src/registry.rs`), and the new `website/src/paths-filter.test.mjs`, run by the same step, fails when a path read there (a `read('...')` literal, a join on the repository root, a `../../` path) matches no `pull_request` pattern, when a `read` call has no literal path, or when the `push` list differs from it. Amended (#141 review): the scan now reads every `.mjs`, `.js`, `.ts`, `.astro` and `.mdx` file under `website/` (outside `node_modules` and build output), resolves a `../` literal (an import, a `new URL`) against its own file, and fails on a file-system call whose path is in none of the forms it lists; it is static and does not follow variables, which `website.yml`'s comment and ADR-0063 item 5 now say. Its paths reader keeps an item with a trailing `# comment` and fails on an indented line it cannot read instead of ending the list there, a whole-segment `**` also matches no segment, and the `Status and layer tests` step also runs `website/src/progress.test.mjs`, which no workflow ran.
- [x] 10.1.53 (v1.0) `persia-engine`'s integration tests read files outside the crate (found in 10.1.41): they `include_bytes!`/`include_str!` inputs under `fuzz/corpus/`, so they do not build from a published engine crate. Move the inputs into the crate (or the tests into a never-published crate, as 10.1.42 did for the guides) before its `publish = false` is flipped, and extend the include check to every published crate. [T] No include in a crate's sources reaches outside its directory (the 10.1.42 repo check, for every crate) Done as: the inputs were copied, not the tests moved. Those tests check engine internals that a crate on `persia` cannot reach. The engine's 11 included seeds now live in `crates/persia-engine/tests/fixtures/fuzz_seeds/`. After review, persia-format's 14 runtime-read seeds (about 80 KB) moved to `crates/persia-format/tests/fixtures/fuzz_seeds/` too. `fuzz/corpus` stays the source: `fuzz_seed_copies_equal_the_corpus` requires every copy to equal its corpus file byte for byte. The include check (`xtask/src/repo_checks/includes.rs`, now `published_crates_include_nothing_outside_their_directory`) covers every root-workspace member except those whose own manifest writes `publish = false` (or `[]`). It reads the manifests, not a hand list. `xtask` and `persia-testutil` now say so themselves, as `persia-docs` already did. ADR-0063 guides amendment item 6. An include in persia-blob reaching `../../fuzz` fails the check. The remaining gaps (runtime reads of `tests/fixtures`, the stripped `persia-testutil` dev-dependency) are 10.1.55
- [ ] 10.1.54 (v1.1) Spellings the include and guide-name checks miss (found in the 10.1.41 re-review): `#[ path = "…"]` with a space and `#[cfg_attr(…, path = "…")]` in the include check, `<code class="…">` in the guide-name check; none occurs in the tree. [T] Unit cases of each fail their check
- [ ] 10.1.55 (v1.0) Published crates' tests still need files their package does not ship (found in the 10.1.53 review): 10.1.53's include check reads compile-time includes only. (a) `persia-testutil` is `publish = false` and a path-only dev-dependency of `persia-engine`, which `cargo package` strips, so `crates/persia-engine/tests/relax_zero_results.rs` and `relax_quality.rs` do not build from the engine's package; (b) tests of published crates read files outside their directory at run time (`std::fs::read` of `CARGO_MANIFEST_DIR/../../tests/fixtures/...`, the golden files and other inputs read by tests of `persia-format`, `persia-storage`, `persia-blob`, `persia-engine` and `persia-cli`; `persia-format`'s two seed tests read copies since 10.1.53's review). Decide per case before a `publish = false` is flipped: publish `persia-testutil` or gate the tests that use it, and move each runtime input into its crate (as the fuzz seeds were) or the test into a never-published crate, and record it in ADR-0063's 10.1.53 item. [T] A repo check refuses a published crate's test reading a path outside its directory, and the engine's package builds its tests (e.g. `cargo package -p persia-engine` then `cargo test` in the unpacked crate, or a check that no published crate has a dev-dependency that is `publish = false`)
- [ ] 10.1.56 (v1.1) `code.md`'s engine-internals part covers the second half of the page (#140, open question 1): about 175 lines of `persia-format` quotes sit under one marker. Explicit `persia::` paths in it are still checked, but nothing else is. Split it into one marked part per section, so each part covers only its engine quotes and the prose between them goes back under the name check. [T] `guides_name_only_the_public_api` scans `code.md`'s prose between the engine quotes, and a facade name invented there fails it
- [ ] 10.1.57 (v1.1) The guide doctests compile twice (#140, open question 2): the workspace run sees `persia-cli`'s `internal` feature, so only the separate `cargo test -p persia-docs` step compiles the samples against the public feature set. Decide between keeping both runs and excluding `persia-docs`'s doctests from the workspace run (e.g. `--exclude persia-docs` on the workspace doctest line), and record the choice in ADR-0063's guides amendment. `persia-docs` is never published, so this does not block a release. [T] CI compiles each guide sample against `persia`'s public feature set only, in exactly the runs the ADR names
- [ ] 10.1.58 (v1.0) Fact-check the guide prose rewritten in 10.1.41 line by line (#140, open question 3): the implementer verified the claims from forks against the code, and the reviewer spot-checked defaults, limits and CLI lists. Not every sentence of the ~1,200 added lines was checked against the code and SPEC. [T] Each page under `website/src/content/docs/guide/` has been read against SPEC and the code, and every wrong claim is fixed or filed as a leaf
- [ ] 10.1.59 (v1.1) `shown_samples_equal_a_doctest`'s `requires_toml` arm has no unit case (found in the 10.3.3 re-review): deleting the README's `toml` block fails the real-page check, but deleting the arm that reports it leaves every test green, since the real pages all have the block. Move the `toml` comparison into a function and test it with and without the block. [T] Removing the `requires_toml` arm fails a unit test
- [ ] 10.1.60 (v1.1) Read calls `website/src/paths-filter.test.mjs` does not see (found in the #141 follow-up re-review): its `FS_READ` list has the sync `node:fs` names plus `readFile` and `createReadStream`, so `node:fs/promises`' `readdir`, `stat`, `lstat`, `access`, `open` and `opendir`, and aliased imports (`readFile as rf`), are not matched; and a join on a repo base records only its first literal (`path.join(root, 'crates', 'persia', 'src', 'lib.rs')` gives `crates`), which fails safe but names the wrong path. None occurs in the tree. [T] Unit cases of each fail the coverage test until their path is in `website.yml`'s filter, and a multi-literal join is reported as its full path
- [ ] 10.1.61 (v1.1) An async search's timeout does not count its time in the pool's queue (found in 10.1.44, ADR-0063 amendment item 6): `AsyncCollection::search` starts the timeout when its job starts, so a caller's budget (a gRPC deadline in `persia-server`) can be overrun by the wait. Decide whether the timeout starts at the call, and how the job learns the instant on the database's clock. [T] An async search queued behind a busy pool past its timeout fails with `Cancelled` without running (an injected clock, no sleep)
- [ ] 10.1.62 (v1.1) Cancellation and a passed timeout share `ErrorCode::Cancelled` (found in 10.1.44): today a sync search has no other interrupt and only a dropped async future cancels, so the code is unambiguous; once one call can be both cancelled and timed out (the server, an SDK context), decide whether a `DEADLINE_EXCEEDED` code is wanted (it changes `ErrorCode::ALL` and the C ABI table in `docs/sdk/c-abi.md`). [T] Each cause maps to its decided code, and the C ABI table test passes
- [ ] 10.1.63 (v1.1) Static rank pruning uses one bound per view (found in 10.1.45): `M` is the contribution of the segment's largest key, so a block's bound adds the column's maximum, not that block's; a ranked collection pays about 2x on a term search (`docs/benchmarks.md`), from this loose bound and one column read per scored document. Bound each block by its own documents' largest contribution, or another sound scheme. [T] The pruned page equals the exhaustive one (the planning proptest), and a bench shows the ranked rows before and after
- [ ] 10.1.64 (v1.1) A memtable view takes no threshold in a static-ranked collection (found in 10.1.45): it has no FOR block bounds, so every match is scored. A bound over the memtable's keys (kept as documents are added, or read once per view) would let it prune. [T] A memtable-only ranked search returns the exhaustive page and scores fewer documents
- [ ] 10.1.65 (v1.0) The relaxation cascade's differential suite has no static rank (found in 10.1.45): `crates/persia-engine/tests/differential.rs` (invariant 5) runs the cascade without `ranking.static_rank`, and the reference L5 orders by recency only; static-ranked totals are checked by `tests/search.rs` and `tests/static_rank.rs` alone. Draw a static rank in the cascade's schemas and model it in the reference cascade, L5 included. [T] The cascade's pages and steps equal the reference's with a static rank drawn
- [ ] 10.1.66 (v1.1) The first static-rank bound of a segment is read whole or not at all (found in the 10.1.45 review): the walk over the rank column's FOR blocks steps under the request's timeout and keeps nothing when cut, so a segment whose walk takes longer than callers' timeouts (millions of documents behind a cold cache or object storage) never caches its bound and every pruned search on it fails `Cancelled`. Keep the walk's progress across searches, or record the column's largest key when the segment is written (an on-disk change: SPEC, format version or feature flag, ADR, golden file). [T] Successive searches each under a short timeout end with the bound cached, or a segment written with it reads no block for it
- [ ] 10.1.67 (v1.1) The static rank's weight is not normalized against BM25F (found in 10.1.45, SPEC §7.2's note): a weight that lifts ties in one corpus can override the text score in another. Give guidance in `ranking.md`, or normalize (possibly with function score, M32). [T] The decided rule is in SPEC §7.2 and `ranking.md`, with a test of its example

## 10.2 Concurrency semantics
- [x] 10.2.1 `Send + Sync` handles; reader snapshots; single-writer errors Done as: every handle `Send + Sync + 'static` (`BlobReader` `Send`); lock-free reads of the published snapshot; `Db::refresh` for a read-only `Db`, one at a time; a second writer is `Conflict`; `tests/handles.rs`.
- [x] 10.2.2 Read-your-writes guarantees documented and tested Done as: SPEC §8.4 "Read-your-writes" and the `Db` docs; `tests/read_your_writes.rs` (each durability, another thread, a read-only refresh, a property test against a model with flushing memtables).
- [x] 10.2.3 Background threads lifecycle (flush, merge) and clean shutdown Done as: one `persia-maintenance` thread per read-write `Db` (`maintenance.rs`: a pure `decide` schedule, rounds of `tick` + `merge_step`), `close` joins it, `Drop` waits 5 s then detaches; `tests/maintenance.rs`, `tests/db_process.rs` (ADR-0064).
- [x] 10.2.4 Read handles use mapped reads where the container is on a local disk (`Container::enable_mapped_reads_if_local`, ADR-0021; found in 2.3.10): on by default, `OpenOptions` can force `pread`; never on the writer's handle (a live mapping blocks tail truncation on Windows, ADR-0017) Done as: done by 10.1.23 and wave 7's #127/#128: on by default for read-only handles on local disks (container and blob pack), `OpenOptions::mapped_reads(false)` forces `pread`, the writer never maps; the Windows default is `Never`, a deliberate safety default, follow-up tracked in 10.1.22 (v1.1).
- [ ] 10.2.5 One block cache per `Db` (found in 5.3.15): every segment the database opens reads through one `BlockCache` (SPEC §10.6) of `OpenOptions` size (default from 15.2.4), not the 8 MiB process-wide `BlockCache::shared`; its stats exposed for metrics
- [x] 10.2.6 (v1.0) Callers never call `tick()` or `merge_step()`: commits and merges run on background threads, and `close` joins them (complements 10.2.3; SPEC §8.2, §8.5). The engine's `Database::tick` and `Database::merge_step` stay how those threads drive a `Database`. [T] A program that only puts and closes does not name `tick` or `merge_step`, and `close` returns after the background threads have stopped Done as: the maintenance thread drives `tick` and `merge_step`; `tests/lifecycle.rs` (a program that only opens, puts and closes; no `persia-` thread after `close` or a drop), SPEC §8.2.
- [x] 10.2.7 (v1.0) A create whose pack fails to open removes the new container after the database (and its writer lock) is dropped (`internal/documents.rs`, `finish_create` and `remove_created_container`, found in the 10.1 review): another open can take the lock in between and open, or recreate, the file being removed. Remove it while still holding the lock on Unix (unlink before the drop), and on Windows, where an open file cannot be unlinked, mark it so a racing open refuses it, or delete on close. [T] A create whose pack open fails, raced by a second open, never leaves that open with a removed file or a half-created database Done as: `remove_created_container` (`crates/persia/src/internal/documents.rs`) removes the container before the database, and its writer lock, drop (on Unix the lock on the file, on Windows the `<name>.lock` beside it), then syncs the directory; a racing open stops at the lock, or at its path lookup once the name is gone, and a racing create gets `AlreadyExists`. Tested at both stages with a lock probe, on real threads, and across a SimFs crash; CI's Windows job runs the module's tests.

## 10.3 Docs & examples
- [ ] 10.3.1 Rustdoc with doctests for every public item
- [ ] 10.3.2 `examples/`: quickstart, products-search, blobs, relaxation demo
- [x] 10.3.3 README quickstart (copy-paste works, tested in CI) Done as: an xtask check, not a tempdir in the README. A tempdir would make users add `tempfile`, and the sample would no longer leave `app.persia` behind. `shown_samples_equal_a_doctest` (`xtask/src/repo_checks/samples.rs`) requires the README's Quickstart `rust` block to equal the getting-started guide's first sample as shown, and the `toml` block before it to equal that page's first one. Getting-started's first sample already runs as a `persia-docs` doctest. Its only allowed hidden lines are comments and the one `let dir = tempfile::tempdir()…;`, and its single `dir.path().join("app.persia")` reads as `"app.persia"`. The twin must run (not `ignore`/`no_run`) on a wired page. So README and getting-started cannot drift apart, and no doctest calls `set_current_dir`. The README's `main` now returns `Result<(), Box<dyn std::error::Error>>` and ends with `db.close()?; Ok(())`, as the guide's does. persia's manifest has no `readme` field, so packaging is unchanged. Pasting into a fresh `cargo new` was checked by hand. ADR-0063 guides amendment item 7. A broken call in the README fails the check

## 10.4 CLI (`persia-cli`)
- [x] 10.4.1 `persia inspect|dump|verify|compact|stats` commands Done as: the existing `container`, `store`, `inspect`, `dump segment` and `verify` commands unchanged, plus `persia stats <path> [--json]` (a read-only open, no lock: last commit, live and dead space, WAL after the last commit, blob pack size, per-collection schema, documents and segments), `persia compact <path> [--json]` (`Database::compact`, read-write, space before and after; recovery in its open may commit a WAL backlog over the default memtable) and `persia dump documents <path> --collection <name>` (JSON Lines in the SPEC §6.4.1 output form, storage order). Exit statuses 1, 3 and 4 as before, tabled in the crate docs and tested per command over sound, missing, not a container, damaged and writer-held files; `--json` keys in the README (SPEC §4.6, §8.6). Tests: `tests/commands.rs`, unit tests in `stats`, `compact`, `documents`.
- [x] 10.4.2 `persia import` (JSONL) and `persia search` for quick experiments Done as: `persia import <path> --collection <c> [--input <file>] [--schema <file>] [--json]` and `persia search <path> --collection <c> (--query <text> | --request <json>) [--json]`, through the embedded API (SPEC §8.6). Import reads JSON Lines (a file or stdin) with the bounded reader `persia::internal::jsonl` (16 MiB a line, CRLF, blank lines skipped, a BOM refused at line 1), puts each line logged `Buffered` and commits at least at the end; `--schema` creates the database and the collection. Prefix rule: the first bad line (not UTF-8, too long, not JSON, not a document of the schema, a put refused) stops the import naming its 1-based line, with the lines before it committed and nothing from it on, so it resumes from that line; a kill leaves a prefix. A dangling `BlobRef` is refused (the facade's put, SPEC §9.6), naming the line and the `blob_id`. Search opens read-only and prints rank, score and document, or the response JSON of SPEC §7.5.2. Tests: `tests/import.rs` (bad-line kinds, dump round trip, dangling and mismatched `BlobRef`, BOM, SIGKILL then resume, search), `tests/commands.rs` status table, unit and property tests in `import` and `persia::internal::jsonl`, fuzz target `jsonl_lines`.
- [x] 10.4.3 `persia bench` quick micro-benchmark entry point Done as: `persia bench [--dir] [--docs] [--queries] [--seed] [--json]` indexes a seeded synthetic corpus into a new database in a scratch directory it creates and removes, commits, runs one untimed warm-up query and a seeded mix of five query kinds, and reports documents per second, the commit time and per kind the hits, the queries the relaxation budget truncated and the latency p50/p90/p99/max (nearest rank, computed in the CLI), as text or one JSON object; help and output say the numbers are local guidance only (SPEC §16.3). Tests: `tests/bench.rs`, unit tests in `bench`, `bench::corpus`, `bench::percentile`, `bench::workdir`.
- [x] 10.4.4 (v1.0) `persia compact` on a database whose schema names a registered analyzer: the command has the built-in analyzers only, and `Database::open` needs every analyzer the schemas name, so it exits 1 today. Needs an engine entry point: a read-write open that skips analysis for a compaction (as `ReadOnlyDatabase::open_skipping_analysis` does for reads), or a compaction that takes the writer lock without replaying the WAL (found in 10.4.1) Done as: `persia_engine::db::CompactOnlyDatabase` (ADR-0068, SPEC §4.6, §6.2.2, §8.6): the writer's open with no-token stand-ins for the analyzers its registry lacks (`skipped_analysis()`), refusing before anything is written any flush while a skipped collection holds a replayed record (`DatabaseError::AnalysisSkipped`, code `Unsupported`), so those records stay WAL records for the next writer to index; it offers `compact`, `space_usage` and `seq` only, and with every analyzer it is the writer's open. `persia compact` always opens with it; statuses and `--json` unchanged. Tests: `persia-engine/tests/compact_only.rs` (with and without a backlog, refusals with the file unchanged, a plain flush before skipped records, two collections sharing an analyzer, the lock, fault and crash sweeps over the compaction and over the open's own writes, a retired cut commit), doctests, a recovery unit test, `persia-cli/tests/commands.rs`.
- [x] 10.4.5 (v1.0) `persia gc`: blob garbage collection from the command line (mark, sweep and pack rewrite, SPEC §9.5), with its grace period and a report of what it deleted and what a reader lease held back; `persia compact` rewrites the container only and leaves `<name>.persia.blobs` alone (found in 10.4.1) Done as: `persia gc <path> [--grace <dur>] [--json]` through `persia::internal`'s `BlobDatabase::gc` (mark, sweep, pack rewrite at the default ratio; SPEC §9.5), opening read-write with the pack (status 1 while a writer holds it; built-in analyzers only, 10.4.7). The grace (default 24 h) is checked by `GcOptions::validate` before the open, so a value below the 2 h floor exits 1 with every file unchanged. The local pack has no ages: it drops what the mark does not keep at once, so `young` is 0 locally. It reports what was marked, deleted and kept, the rewrite, the pack's size before and after, dangling references, and the reader leases by state; a lease behind the committed seq holds the sweep back and the report says so (status 0). A database without a pack is a no-op. Tests: `tests/gc.rs`, `tests/gc_processes.rs` (kill -9 at observed points, reader processes, a stopped gc holding the locks, soak), `tests/commands.rs`, unit tests in `gc`.
- [ ] 10.4.6 (v1.1) `persia dump documents` with the blobs: a dump line carries a `BlobRef`, not the bytes, so a dump read back by `persia import` (10.4.2) into another database has dangling references. Decide the export form (a directory of blobs by id beside the JSON Lines, or a flag that refuses documents with blobs) and implement it with `import` (found in 10.4.1)
- [x] 10.4.7 (v1.0) `persia gc` on a database whose schema names a registered analyzer: GC's mark runs in the writer's open (`BlobDatabase::open`, `Database::open`), which needs every analyzer the schemas name, so the command (built-in analyzers only) exits 1 today. The compaction-only open of ADR-0068 offers no mark (`BlobReferences`, `DocumentLog`). Decide whether a mark-only writer open with its stand-ins is safe (the references need no analysis, but its replay may need to flush, which it must refuse as ADR-0068 does), add it, or keep the refusal (found in 10.4.5) Done as: safe, so added (ADR-0069): `persia_engine::db::MarkOnlyDatabase` runs the compaction-only open's recovery (`Database::open_skipping_analysis`, shared with `CompactOnlyDatabase`: a no-token stand-in per missing analyzer, and `AnalysisSkipped` refusing, in `plan_flushes` before any segment is built or id taken, any flush while a skipped collection holds a replayed record) and offers only the mark (`referenced_blobs`, which reads documents, never a term), `seq`, `writer_proof`, `container_path`, `skipped_analysis`. A skipped collection's records stay WAL records and memtable documents, so they are marked and their blobs kept. `persia::internal`'s `BlobDatabase::open_mark_only`; `mark`/`gc` need only `BlobReferences`, and `blobs`/`put` still need a `DocumentLog` (compile_fail doctests). Sweep, leases (judged by the committed seq after the open's own commits) and pack rewrite unchanged (SPEC §9.5). `persia gc` uses it; a refused open exits 1 and says to commit with the application, then retry. Tests: engine `tests/mark_only.rs` (mark equals the writer's, refusals, cut commit, crash sweeps over its replay-time flushes and retiring commit, repeated refusals), `persia/tests/reader_lease.rs`, `persia-cli/tests/gc.rs`, `gc_processes.rs` (kill -9 during the open's flush, a reader process, a seeded soak with a registered analyzer), `gc.rs` unit tests of the open hints. Repeated refusals re-flush: 10.4.8.
- [x] 10.4.8 (v1.0) A refused analysis-skipping open re-flushes what it flushed (found in the 10.4.7 integration tests): when a compaction-only or mark-only open (ADR-0068, ADR-0069) is refused with `AnalysisSkipped` after its replay flushed other collections, `wal_start` stays at the chunk the skipped collection holds, so the next refused `persia gc` or `persia compact` replays the records from that chunk on and, when they reach the flush threshold before the skipped record, flushes them again into new segments (the same documents; nothing lost or resurrected). It needs a crashed application whose uncommitted tail crosses the open's threshold before a skipped record, and is repeated only while the records between `wal_start` and that record reach it (in the test, a small memtable: +16,488 bytes per refusal); the container grows until the application commits, and merges and compaction reclaim it. Decide whether to bound it (e.g. refuse before the first replay-time flush when a skipped collection holds a later record, or move `wal_start` past fully flushed chunks) and record it in ADR-0068/0069 [T] N refused runs grow the container by at most one backlog Decided (owner, 2026-11-27): before its replay, an analysis-skipping open pre-scans the WAL from `wal_start` once (the replay's decoder, applying nothing) and refuses with `AnalysisSkipped` if any pending record (one the replay would apply) belongs to a collection with a skipped field, wherever a flush would be due, so a refused open writes nothing but the tail repair; the `plan_flushes` guard stays as a backstop (SPEC §8.6, ADR-0068/0069 amendments). Done as: `Database::recover_as` with `Analysis::SkipUnregistered` (both `CompactOnlyDatabase` and `MarkOnlyDatabase`) pre-scans the WAL from `wal_start` with the replay's own chunk list and decoder (`wal/replay.rs` split into `decode` and apply), after the container's recovery and before any replay, segment id or retiring commit. It refuses with `AnalysisSkipped` (naming the first skipped field, in field order) when a collection with a skipped field has a pending record: one that decodes, before the first record that does not (SPEC §8.6; re-replayed records a segment already holds count, a drop or schema change moves `wal_start` to the end of the log, a memtable-rejected record does not stop the scan, so the rule errs on refusing). A chunk it cannot read (I/O, a frame that does not decode) fails the open at once with nothing written; a record-level decode error is left to the replay, so a damaged WAL reports the writer's error (after a pending record, now the damage instead of `AnalysisSkipped`). A refused open writes only the tail repair. The `plan_flushes` guard stays as a backstop. Accepted trade-off: `persia gc`/`compact` exit 1 after a crash leaves such a record, until the application opens and closes the database (flipped tests listed in the PR). ADR-0068 and ADR-0069 amended. Tests: `persia-engine/tests/analysis_prescan.rs` (a property test against a model with drops, recreates and schema evolution; both orderings refused repeatedly with only a bare open's bytes; tail repair fault sweep; read failure, undecodable frame, damage before and after a pending record; segment-held records), `compact_only.rs`, `mark_only.rs`, recovery unit test of the guard, `persia-cli/tests/gc.rs` (repeated refused gc and compact write nothing, either order), `gc_processes.rs` (kill -9 during the pre-scan, a soak of killed application processes, kill -9 of the app's open and close after a refusal and of an accepted compact), the `database_open` fuzz target.

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
- [ ] 11.4.8 (v1.0) `ReadOnlyDatabase::relaxation_stats`: a read-only handle's searches count into its snapshot's collections (`CollectionSnapshot::relaxation_stats`), but it has no database-wide sum as `Database` has; add one (with `ReadOnlyDatabase::refresh`, ROADMAP 6.6.6, keeping the counts of the collections it reloads) so the server's metrics (11.4.4) can serve from a read-only handle (found in 8.3.3)

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
- [ ] 13.2.5 (v1.0) Remote blob GC under reader nodes (extends 13.2.4; SPEC §9.5, ADR-0052): a stateless query node that lags behind the writer may find the blob of a deleted document gone after the grace period; reader leases or a low-water snapshot `seq` the sweep honors (found in 9.5.2)
- [ ] 13.2.6 (v1.0) `persia verify` over the remote blob layout (SPEC §9.4, §9.5; ADR-0053): missing referenced blobs and a checksum scan of `blobs/`, as 9.5.3 does for the pack (found in 9.5.3)

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

## 13.7 Blob follow-ups from M9 (local pack, reader leases, GC)
- [ ] 13.7.1 (v1.0) `persia leases <db>` lists the reader leases beside a database's pack (`<name>.persia.blobs.leases/`: id, pid, `manifest_seq`, state live/stale/unknown, against the container's committed seq) and `persia leases <db> --remove-stale` removes those whose lock the command is granted. It runs in a process of its own that holds no reader lease, so a granted lock shows that no other process holds it, also where locks belong to the process (ADR-0019). `--remove <id>` removes one after the operator checked its pid: the way to unblock GC on file systems where the writer cannot judge leases (overlayfs, vfat/exFAT, 9p, virtiofs, NFS, CIFS, FUSE; ADR-0054 item 13; found in 9.2.16)
- [ ] 13.7.2 (v1.0) A two-pass pack sweep (ADR-0054 item 9): today GC does not sweep while any reader lease is behind the committed seq, and every commit or container compaction puts every out-of-process reader behind until it refreshes, so delete, commit, gc never sweeps while such a reader lives and lags. Remember in memory what a held-back round would drop, and drop it in a later round once every live lease is at or past the seq of that round; decide whether a compaction commit, which drops no document, needs to hold readers back at all (found in 9.2.16)
- [ ] 13.7.3 (v1.0) The remote sweep without `strong_list` when both its listings miss a just-written chunk (the residual of 9.5.10): the locked re-list catches a chunk only the first listing missed; a backend whose listings both lag a fresh chunk can still lose a resumed upload's old chunks. Decide whether such a backend needs a delay, a per-upload marker, or is unsupported for resumable blobs (SPEC §9.5)
- [ ] 13.7.4 (v1.0) A snapshot kept across `ReadOnlyBlobDatabase::refresh` is not protected (ADR-0054 item 4, SPEC §9.5): its `Arc` outlives the replaced `ReadOnlyDatabase`, the reader lease then names the newer documents only, and after a GC a blob only that snapshot shows reads as `BlobNotFound` (never other bytes). Track such snapshots with weak references, as the writer's mark does (ADR-0052 item 2), and keep the oldest held seq in the record (found in 9.2.16)
- [ ] 13.7.5 (v1.0) Run `persia-blob`'s pack lock and reader-lease tests on the `windows-latest` CI job (today it runs only `persia-format` and `persia-storage`): `lease_format.rs`, `lease_scan.rs`, `pack_writer.rs`, and a real-file test of the `<id>.lease.lock` sidecar path through `Vfs::lock_beside`. That covers a take, a GC judging a live and a stale lease, the delete-pending `PermissionDenied` retry after a stale removal, the orphan-sidecar sweep leaving a sidecar alone while its lease exists, and the pack lock's `lock_beside` branch (ADR-0054 items 5, 8 and 12; found in the 9.2.15 and 9.2.16 reviews)
- [ ] 13.7.6 (v1.0) `PackStore::reopen` carries mapped reads over to the reopened pack; today it starts unmapped and the caller re-enables them (found in 9.2.13)
- [ ] 13.7.7 (v1.0) The public API shows whether a read-only handle holds a reader lease (`ReadOnlyBlobDatabase::lease()` is `None` when the directory refused writes, SPEC §9.3), so an application can tell an unprotected reader; the facade does not exist yet (M10; found in 9.2.16)

---

# M14 — SDKs (Rust & Go)
Deps: M11 (stable proto); M13 for shard-awareness.

M14's scope now includes TypeScript. The heading stays "SDKs (Rust & Go)"; the title that names the scope is "SDKs (Rust, Go & TypeScript)". The network client is 14.5 (`@persiadb/client`). Embedded TypeScript stays 16.3 (`@persiadb/node`).

M14's scope now also covers the embedded (in-process) SDKs. The heading stays "SDKs (Rust & Go)"; the title that names the scope is "SDKs (Rust, Go, TypeScript & embedded)". That work is 14.6. It depends on M10 (the `persia` facade) and not on M11. Rust and Go network clients are 14.2 and 14.3, compatibility is 14.4, and the TypeScript network client is 14.5. Those still depend on M11 and M13. The v1.0 schedule for `@persiadb/node` is 14.6.6 and 14.6.7, which pull 16.3.1–16.3.5. 16.3 stays that duplicate list and is checked when those leaves are done. The M16 leaves that stay on v1.1 are 16.4.1 and 16.6.

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

## 14.6 Embedded SDKs (in-process)
Depends on M10 (the `persia` facade), not on M11. The design is ADR-0018 and `docs/sdk/` (C ABI, Go, TypeScript, distribution). Parity with the gRPC SDKs is the 14.1 runner harness, for the in-process operations those scenarios name (CRUD, search and filters, relaxation, facets, blobs, errors, read-your-writes). TLS, retries, deadlines, and shard routing stay on 14.2, 14.3, and 14.5.

- [ ] 14.6.1 (v1.0) C ABI crate `persia-ffi` (crate types `lib`, `staticlib`, `cdylib`) depending only on `persia`, and a committed `persia.h` from `cargo xtask ffi-header` (no `build.rs`). Safe API over `Db` and `Collection`: open, close, create and open a collection, put, upsert, delete, get, put_batch, commit, search, blobs. C exports catch panics and poison that handle afterwards. Stable error codes mapped from the Rust error type; corruption is an error; a mapped-page fault still ends the process. Record the crate edge in `ALLOWED` and the C-export module in `UNSAFE_ALLOWED`. Pulls 16.1.1–16.1.4 into v1.0 (ADR-0018, ADR-0003, ADR-0010, `docs/sdk/c-abi.md`, SPEC §8.1, §9.6, §13.1, §17)
- [ ] 14.6.2 (v1.0) [T] A document and a search round-trip through the C API; a panic in the core returns an error and poisons that handle; hostile UTF-8 is an error. Pulls 16.1.5 into v1.0 (`docs/sdk/c-abi.md`)
- [ ] 14.6.3 (v1.0) Embedded Go SDK: module `github.com/tomiok/persiaDB/sdk/persia`, package `persia`, cgo linked to `libpersia`. `Open`, collections, put, get, delete, search, blobs, `context.Context`, and `errors.Is` sentinels, with the cancellation check `docs/sdk/go.md` already describes. The call surface covers the in-process operations in the 14.1 scenarios. Pulls 16.2.1–16.2.2 into v1.0 (ADR-0018, `docs/sdk/go.md`, SPEC §13.1)
- [ ] 14.6.4 (v1.0) [T] Go conformance adapter for the 14.1 scenarios with `transport: embedded`, the same scenarios the gRPC SDKs run through the 14.1 runner harness. `go test` with the race detector; a file written by Go opens in Rust and returns the same hits. Pulls 16.2.4–16.2.5 into v1.0 (SPEC §13.2, `docs/sdk/go.md`)
- [ ] 14.6.5 (v1.0) Optional build tag `persia_purego`: on Unix, `github.com/ebitengine/purego` dlopens the prebuilt shared library with `CGO_ENABLED=0`; on Windows amd64, `syscall.LoadLibrary` and `GetProcAddress` load `persia.dll`, because `purego.Dlopen` is not built for Windows. Same Go API as the cgo build. cgo with the static `libpersia.a` stays the default. The build does not fetch the shared library; the caller places it beside the binary or names it with `PERSIA_LIB`, a Go-SDK-only runtime variable read by `Open` (not a server `PERSIA_*` knob, and not a failure before `main`). The platform matrix is the Go matrix in `docs/sdk/distribution.md`. The C ABI stays purego-friendly: opaque handles, pointer+length buffers, no struct-by-value and no callbacks. wazero stays rejected. Checksummed cdylibs for this tag are published by 14.6.10. Pulls 16.2.6 into v1.0 (ADR-0018, `docs/sdk/go.md`, `docs/sdk/distribution.md`). [T] `go test -tags persia_purego` with `CGO_ENABLED=0` opens a file the cgo build wrote and returns the same hits
- [ ] 14.6.6 (v1.0) Embedded TypeScript SDK `@persiadb/node`: N-API addon outside the Cargo workspace, linking the `persia-ffi` rlib. Promises, `PersiaError.code`, and `get` of a missing id returns null. Workers stay as `docs/sdk/typescript.md` describes them. The call surface covers the in-process operations in the 14.1 scenarios. Pulls 16.3.1–16.3.2 into v1.0 (ADR-0003, ADR-0018, `docs/sdk/typescript.md`)
- [ ] 14.6.7 (v1.0) [T] `@persiadb/node` conformance adapter, the same 14.1 scenarios as 14.6.4. Node, and Bun or Deno, opens a file written by Rust and returns the same hits. Pulls 16.3.4–16.3.5 into v1.0 (SPEC §13.2, `docs/sdk/typescript.md`)
- [ ] 14.6.8 (v1.0) Embedded Python package (`sdk/python`): bind `persia.h` with cffi or ctypes. Installing the package does not require a Rust toolchain. Open, collection, put, get, delete, search, blobs, and commit. Exceptions carry the stable `code` from 14.6.1. Release the GIL around the native call. One database still has one writer (SPEC §8.4). A call already inside the engine does not cancel. The call surface covers the in-process operations in the 14.1 scenarios. SPEC §13 names this package in 14.6.11, in the same release. Pulls 21.1.1–21.1.3 into v1.0 (ADR-0018, `docs/sdk/c-abi.md`, `docs/sdk/distribution.md`)
- [ ] 14.6.9 (v1.0) [T] Python conformance adapter (`transport: embedded`), the same 14.1 scenarios as 14.6.4. A file written in Python opens in Rust and returns the same hits. Pulls 21.1.5 into v1.0 (SPEC §13.2, `docs/sdk/c-abi.md`)
- [ ] 14.6.10 (v1.0) Prebuilt checksummed libraries and CI: static libraries per OS and architecture and a documented source build; cdylibs and their checksums for `persia_purego`; prebuilt `.node` binaries per platform and an npm pack dry-run; Python wheels that ship the prebuilt library and a `pip` dry-run. CI builds that matrix. Users of the engine do not run it. Compatibility table for the Go module, `@persiadb/node`, and the Python package against the core tag they were built from. File format, C ABI, and package versions stay distinct numbers. Pulls 16.2.3, 16.3.3, 16.4.2, 16.4.3, and 21.1.4 into v1.0 (ADR-0018, SPEC §15, `docs/sdk/distribution.md`). [T] The dry-runs install without a Rust toolchain, and each published artifact has a checksum beside it
- [ ] 14.6.11 (v1.0) SPEC §13 names the embedded Python package beside the Go and TypeScript bindings, on its own path, and a short guide page. That package is 14.6.8. Pulls 21.3.1 into v1.0 (SPEC §13.1, ADR-0018)

---

# M15 — Benchmarks, hardening, v1.0
Deps: M1–M14 (can start benchmarking early after M7).

## 15.1 Benchmark harness
- [ ] 15.1.1 Dataset acquisition scripts (public corpora; pinned versions/checksums)
- [ ] 15.1.2 Workload definitions (query logs: head/tail, typo-heavy, filtered, facet)
- [ ] 15.1.3 End-to-end harness (embedded and server modes), latency histograms (HdrHistogram), throughput
- [ ] 15.1.4 Comparison methodology doc + optional adapters to compare other engines (fair configs, published scripts)
- [ ] 15.1.5 CI regression gate (> 5% slowdown fails) with stored baselines
- [ ] 15.1.6 Judged queries proximity can decide: in the 7.8.2 corpus every multi-word query matches 1 to 3 documents under AND, and only 5 of those matches (4 queries) have their words apart, none reordered by the bonus, so the nDCG gate (7.8.4) did not move with 7.3.5. Add documents that hold a query's words both together and apart (and long bodies where they are far), judged, so the gate measures proximity; record the new baseline with the bonus on and off, and decide from it whether proximity turns on by default (off since 7.3.5, ADR-0036) (found in 7.3.5)

## 15.2 Performance work (driven by profiles)
- [ ] 15.2.1 SIMD bit-unpacking (runtime feature detection) behind safe wrappers
- [ ] 15.2.2 Postings decode & scoring hot-path allocation audit
- [ ] 15.2.3 FST/term-dict memory & mmap layout tuning
- [ ] 15.2.4 Cache sizing guidance and defaults from measurements
- [ ] 15.2.5 Disjunction default: on `search_top10_pruning` `MaxScore` beats block-max WAND (`or` 902 µs vs 1.17 ms, `or_rare` 270 vs 432 µs); try block bounds in `MaxScore` (Lucene 9.6's block-max MaxScore) and fewer pivot sums in WAND, then pick the default by benchmark and update SPEC §7.3 (found in 7.4.4); `MaxScore` still scans every essential clause per candidate and sums every bound per lookup, O(clauses²): 41.6 ms vs 24.5 ms for block-max WAND on `search_top10_pruning/or50` (7.4.7). (was 7.4.9)
- [ ] 15.2.6 (v1.0) Remote blob sweep at scale: page the `blobs/uploads/` listing and read the mark's manifests in parallel (SPEC §9.5; found in 9.5.2)
- [ ] 15.2.7 (v1.0) Re-measure `search_relaxed/l0`, `l0/compiled` and `l1` with a profile: after 8.1.16's cache they measured 4–6% slower, though none of them reaches the cache and no new work is on their path (PR #123 recorded a likely code-placement effect, not a proven cause; the residual was accepted for now). Find the cause, or show it is layout noise (the same code under another link order or function alignment), before the release benches of 15.1 (found in 8.1.16)

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
| v1.1 | M16 Embedded SDKs, M17 Highlighting and snippets, M18 Autocomplete, M19 Import, M31 Persia Studio, M32 Ranking rules, M33 Search features, M35 Formal verification |
| v1.2 | M20 Encryption at rest, M21 Python SDK, M34 Backup, export, and query analytics |
| v2.0 | M22 Multi-tenant, M23 Persia Cloud, M24 CDC connectors, M25 Web dashboard |
| v2.1 | M26 UI components |
| v2.2 | M27 AI search |
| v2.3 | M28 On-device |
| v3.0 | M29 Geo, M30 Enterprise |

The embedded SDKs core moved to v1.0 via 14.6 (inside M14): `persia-ffi` and the committed `persia.h`, the embedded Go SDK (cgo and `persia_purego`), `@persiadb/node`, the embedded Python package, and their prebuilt checksummed libraries and CI. SPEC §13 names that Python package in 14.6.11. The rows above are unchanged.

Two orderings differ from a straight list of the features. Multi-tenant is M22 and Persia Cloud is M23, so the hosted service can depend on one file per tenant. v2.1, v2.2, and v2.3 are separate targets so the UI kit does not wait on embeddings, and embeddings do not wait on a portable storage backend. Geo (M29) has no dependency on M26–M28; it stays on v3.0 beside the enterprise format work, and it can move earlier later without renumbering M16–M28. Encryption (M20) is a dependency of Persia Cloud. That only orders work inside the table above: v1.2 still comes before v2.0.

M32 is ranking rules (v1.1). Its BM25+/BM25L leaf (32.3.1) is v1.2 and ships only if the nDCG baseline justifies it. M33 (v1.1) is the search features v1.0 does not cover: CJK segmentation and pluggable tokenizers, reindex with an alias swap, delete-by-query, numeric and range facets, and distinct/group-by. M34 (v1.2) is user-facing backup and export, and analytics of zero-result queries. The read-only WASM build Persia Studio loads is 16.6.

---

# M16 — Embedded SDKs (target: v1.1)
Deps: M15 (format 1.0 and the M10 facade).

The design is ADR-0018 and `docs/sdk/` (C ABI, Go, TypeScript, distribution). This milestone schedules that design. It does not restate it. Network clients stay M14. The browser package is M28. A read-only WASM build for Persia Studio is 16.6 (v1.1); it is not the M28 package.

14.6 schedules these leaves for v1.0, so they are not implemented twice: 16.1.1–16.1.4 are 14.6.1, 16.1.5 is 14.6.2, 16.2.1–16.2.2 are 14.6.3, 16.2.4–16.2.5 are 14.6.4, 16.2.6 is 14.6.5, 16.3.1–16.3.2 are 14.6.6, 16.3.4–16.3.5 are 14.6.7, and 16.2.3, 16.3.3, 16.4.2, and 16.4.3 are 14.6.10. When that 14.6 leaf is done, check the M16 leaf too. 16.2.6 also names the cdylib checksums 14.6.10 publishes, so check 16.2.6 only once 14.6.5 and 14.6.10 are both done. 16.4.1 and 16.6 are not in 14.6 and stay on v1.1.

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
- [ ] 16.2.6 (v1.1) Optional build tag `persia_purego`: on Unix, `github.com/ebitengine/purego` dlopens the prebuilt shared library with `CGO_ENABLED=0`; on Windows amd64, `syscall.LoadLibrary` and `GetProcAddress` load `persia.dll`, because `purego.Dlopen` is not built for Windows. Same Go API as the cgo build (ADR-0018, `docs/sdk/go.md`, `docs/sdk/distribution.md`). CI publishes those cdylibs and their checksums with the static libraries. cgo with the static `libpersia.a` stays the default. The build does not fetch the shared library; the caller places it beside the binary or names it with `PERSIA_LIB`, a Go-SDK-only runtime variable read by `Open` (not a server `PERSIA_*` knob, and not a failure before `main`). The platform matrix is the Go matrix in `docs/sdk/distribution.md`. The C ABI stays purego-friendly: opaque handles, pointer+length buffers, no struct-by-value and no callbacks. wazero stays rejected. [T] `go test -tags persia_purego` with `CGO_ENABLED=0` opens a file the cgo build wrote and returns the same hits

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
- [ ] 16.6.2 (v1.1) The read-only WASM build also reads blobs from the sidecar pack `<name>.persia.blobs` (SPEC §9.3): it verifies chunk/frame checksums and the blob's SHA-256 id as the native reader does, with no write, no fsync and no lock (SPEC §4.5, ADR-0018). A database without a pack works unchanged. [T] A fixture with blobs: `get_blob` in WASM returns the same bytes as the native engine; a corrupted chunk is `Error::Corrupt`, not wrong bytes

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
21.1.1–21.1.3 are 14.6.8, 21.1.5 is 14.6.9, and 21.1.4 is 14.6.10, scheduled for v1.0 so they are not implemented twice. When that 14.6 leaf is done, check the 21.1 leaf too. 21.2 stays on v1.2. 21.3.1 is 14.6.11.

- [ ] 21.1.1 Package layout. Bind `persia.h` with cffi or ctypes. Installing the package does not require a Rust toolchain (`docs/sdk/distribution.md`)
- [ ] 21.1.2 Open, collection, put, get, delete, search, blobs, and commit. Exceptions carry the stable `code` from 16.1.4
- [ ] 21.1.3 Release the GIL around the native call. One database still has one writer (SPEC §8.4). A call already inside the engine does not cancel
- [ ] 21.1.4 Wheels that ship the prebuilt library, and a `pip` dry-run in CI
- [ ] 21.1.5 [T] Conformance adapter (`transport: embedded`, 14.1). A file written in Python opens in Rust and returns the same hits

## 21.2 Network client
- [ ] 21.2.1 Search, put, and get over the HTTP JSON gateway, with timeouts and the idempotent retries the Rust client documents (14.2.4)
- [ ] 21.2.2 [T] The 14.1 scenarios against `persia-server`, with the same expectations as `persia-client`

## 21.3 Docs
21.3.1 is scheduled for v1.0 as 14.6.11, so it is not written twice. When 14.6.11 is done, check 21.3.1 too.

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
- [ ] 31.1.5 (v1.1) A local database is the `<name>.persia` file plus an optional `<name>.persia.blobs` pack (SPEC §4), not a `database.persia/` directory. The page picks those files (file picker, or the directory that contains them) and passes their bytes to the core

## 31.2 Inspect
- [ ] 31.2.1 Segments and the manifest: segment ids, document counts, deletions, and the commit `seq` (SPEC §5, §8.7). A corrupt segment is an error on that row (SPEC §17), not an empty database
- [ ] 31.2.2 A stored document by id (SPEC §6.4). A missing id is shown as missing
- [ ] 31.2.3 Tombstones for a segment: the sidecar version and the deleted ids (SPEC §8.3). A segment with no sidecar shows none
- [ ] 31.2.4 WAL state: `wal_start`, the frames still needed, and whether they would replay (SPEC §8.2, §8.6). The page does not append a record
- [ ] 31.2.5 [T] On a fixture directory, the segment list, one document, one tombstone, and the WAL range match the engine reading the same bytes
- [ ] 31.2.6 (v1.1) Studio can view a document's blobs via the read-only WASM reader (16.6.2). The page shows the bytes `get_blob` returns. A database without a pack shows none

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

## 32.5 Collection ranking defaults
- [ ] 32.5.1 (v1.1) Collection-level ranking defaults in `ranking`: a default tie-break appended to every sort led by the score (`"tie_break": [{"field": "updated_at", "order": "desc"}]`, pruned as 7.4.11 prunes a request's), and the proximity weight `λ`, or `false` (7.3.5, ADR-0036). Both are schema options, so a new manifest `version` (SPEC §8.7): SPEC, version bump, ADR and golden fixture, one bump for both. A request's own `sort` and `proximity` override them. [T] An old manifest opens with today's behavior; a cursor from a sort with the default tie-break pages exactly; pruned == exhaustive with the default

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

## 33.6 Index sort
- [ ] 33.6.1 (v1.1) `index_sort`: a collection declares a column and a direction (`asc` or `desc`, `missing` first or last) its segments are written in (SPEC §5.4, post-v1). A column sort whose first key is that column in that direction walks each view in docid order and stops once it keeps `k` hits; in the opposite direction it walks each view from its last docid down, so it stops as early. That is the part of 7.5.6 that waits on it, and it covers newest first (`search_request/sort_newest`): today no FOR block can lose in docid order there. The two traversal orders keep ties as §7.3 breaks them (a reverse walk must still rank equal keys by ascending docid). A new segment layout follows the format discipline (SPEC §15). [T] Over an index-sorted segment, a sort in the index direction and one in the opposite direction each give the page an unsorted segment of the same documents gives, ties included, and each visits at most `k` matches per view beyond its ties with the `k`-th

---

# M34 — Backup, export, and query analytics (target: v1.2)
Deps: M8, M10, M15.

User-facing operations for one database. Tenant backups on object storage stay 22.2 (v2.0). The dashboard's zero-result list stays 25.4. The relaxation histogram stays 8.3.3.

## 34.1 Backup and export
- [ ] 34.1.1 (v1.2) User-facing backup and export of one database from the CLI and the embedded API: a copy a later open can search. It does not upload a tenant file (22.2). [T] An exported copy opens and returns the same hits as the source; a crash during the copy does not truncate the source

## 34.2 Zero-result analytics
- [ ] 34.2.1 (v1.2) Analytics of zero-result queries: record queries that stayed empty, including after relaxation (SPEC §7.6). This is the record; the v2.0 dashboard list remains 25.4, and the metric histogram remains 8.3.3. [T] A query that is still empty after relaxation is recorded. A query that matched is not

---

# M35 — Formal verification (target: v1.1)
Deps: M1.

The SPEC §10.4 and §11.3 specs are best written before M12 and M13 implement them, and can be pulled earlier. They do not wait on those milestones.

Machine-checked arguments for the protocols and the parsers. TLC specs live under `formal/tla/`. Kani and Loom harnesses compile only under `cfg(kani)` and `cfg(loom)`, so neither tool is a normal dependency (ADR-0001). The jobs are optional and slow, off the PR suite in `.github/workflows/ci.yml`, the way `.github/workflows/nightly.yml` keeps `slow-tests` off it.

## 35.1 Protocol specs in TLA+
Checked with TLC, specs under `formal/tla/`, run as an optional slow CI job.

- [ ] 35.1.1 (v1.1) Remote commit (SPEC §10.4, ROADMAP 12.3): the commit is `put_if_absent(manifests/<seq+1>.mf)`. `Created` commits, `AlreadyExists` means another writer won, and an error leaves the outcome unknown, so the writer keeps its uploads and reads `manifests/<seq+1>.mf` before it retries or aborts. Safety: no lost commit and no duplicated commit, and the manifest chain advances linearizably under concurrent writers and crashes. Best written before M12 implements 12.3, and can be pulled earlier.
- [ ] 35.1.2 (v1.1) Writer lease and fencing (SPEC §11.3, ROADMAP 13.1): `LEASE` is `{holder_id, fence_token, expires_at}`, renewed by a conditional update, and every manifest embeds `fence_token`. Safety: at most one effective writer, and a stale writer's writes are rejected (readers reject a regressed token, and a stale writer cannot win `put_if_absent` once a newer seq exists). A lapsed lease stops that writer. Best written before M13 implements 13.1, and can be pulled earlier.
- [ ] 35.1.3 (v1.1) Blob GC against put and resume (SPEC §9.5, ADR-0053): no referenced blob or chunk is deleted. The model includes the grace period (default 24 h, at least `2 * MAX_BIND_BUDGET`, §9.4; an object whose age is unknown is never expired) and the 9.5.10 resume race: a resume that stages a new chunk under an old upload, binds, and drops its pins between the sweep's listing of `blobs/uploads/` and the check under the pin set's lock.

## 35.2 Bounded model checking with Kani
Proof harnesses behind `cfg(kani)`. A CI job, optional like the nightly slow suite.

- [ ] 35.2.1 (v1.1) The mmap reader, `MmapFile::map` in `crates/persia-format/src/mmap.rs` (ADR-0010), whose `unsafe` block is the one that calls memmap2: for any offset and length, a range that is inverted, longer than `isize::MAX`, or ends past the file is an error and is not mapped, and a map that succeeds covers exactly that range, so the slice never runs past the file length it checked. The harness proves that check. The `mmap` call stays outside it. A fault on a mapped page still ends the process (ADR-0010).
- [ ] 35.2.2 (v1.1) M1 integer encodings and the compression wrapper, at bounded sizes: `decode(encode(x)) == x`, and decode of arbitrary bytes returns a value or an error and never panics. Encodings: LEB128 (`varint`), zigzag (`Reader::read_varint_i32` / `read_varint_i64`), delta (`delta::encode_in_place` / `decode_in_place`), bit-packing (`Writer::write_packed_block` / `Reader::read_packed_block`), postings blocks (`write_postings_block` / `read_postings_block`, `write_postings_tail` / `read_postings_tail`), and frame-of-reference (`for_codec`). Codecs: `compress::compress_block` / `decompress_block` (`Codec::None` and `Codec::Zstd`), with the caller's max uncompressed length, so a bomb is `Error::Corrupt` and the wrapper does not allocate past that limit (SPEC §5.2, §17, ADR-0004, ADR-0005). The zstd C library stays outside the harness; hostile zstd frames stay on the 1.4.3 fuzz target.
- [ ] 35.2.3 (v1.1) Block framing and the superblock: `BlockHeader::decode`, `BlockTable::open`, `BlockTable::decode_block`, `BlockReader::open`, `Header::decode`, `Superblock::decode`, and `pick_active` (SPEC §5.2, §4.2–§4.3). Corrupt input is an error (`Error::Corrupt` or `Error::CorruptBlock`), never a panic, and a declared length cannot allocate past the bomb limit.

## 35.3 Concurrency checking with Loom
Models behind `cfg(loom)`, dev-only (ADR-0001).

- [ ] 35.3.1 (v1.1) Reader refresh against writer commit, and a pack reopen on `PackReplaced`. `Database::publish` swaps `published: Arc<ArcSwap<Snapshot>>` (`crates/persia-engine/src/db.rs`). `ReadOnlyBlobDatabase::refresh` (`crates/persia/src/internal/documents.rs`) installs the reopened `ReadOnlyDatabase` together with the pack: `PackStore::refresh` returns `PackReplaced` and leaves the file it opened (ROADMAP 9.2.13); `refresh` then calls `PackStore::reopen`, and any other pack error restores the previous documents (SPEC §9.5, §18.10, ADR-0053).
- [ ] 35.3.2 (v1.1) `PinSet` (`persia::internal::pins`, a `Mutex`) against `get_blob` and `put_blob` (SPEC §9.5). `put_blob` pins its `upload_id` before the first chunk write (on resume, before listing) and the `blob_id` before it binds. `get_blob` pins the `blob_id` before the manifest lookup and the upload that manifest names right after. `PinSet::unless_pinned` checks and deletes as one step under the lock, so a pin taken after the mark still protects the object. A referenced blob or chunk is not deleted.
- [ ] 35.3.3 (v1.1) The other lock-free and atomic structures: search `Thresholds` (`store_max` raises an `AtomicU64` with `fetch_max`, `crates/persia-engine/src/db/exec.rs`) and `CancelToken` (`Arc<AtomicBool>`); merge cancel (`Arc<AtomicBool>` in `crates/persia-engine/src/merge/scheduler.rs`); `Jitter::next_u64` (`AtomicU64` `fetch_add`, `crates/persia-storage/src/retry.rs`); and `run` in `crates/persia-blob/src/remote/parallel.rs` (`AtomicUsize` index, `AtomicBool` failure flag: the error is the lowest failing index, and a recorded failure starts no further index).

## 35.4 Docs
- [ ] 35.4.1 (v1.1) An ADR in `docs/adr/`: what 35.1–35.3 verify, what is out of scope (full functional verification with Verus or Creusot, rejected as cost-prohibitive), and how to run TLC, Kani, and Loom. Append a pointer from SPEC §16.1. Do not rewrite that section.

---

# M36 — Search aggregations and derived fields (target: v1.1)
Deps: M5, M7.

Post-MVP analytics over the hits of a search (ADR-0055). These leaves are outside M0–M15, so they do not move the MVP percentage. 36.1.1 is design only and is done before the v1.0 format freeze (M15); it does not wait on M15. The metrics are v1.1. Expressions, index-time extractors, and the API are v1.2.

## 36.1 Format
- [ ] 36.1.1 (v1.0, design only) Reserve the segment feature flag / layout for columnar field values before the format freeze; ADR accepted. Record in SPEC, before M15, the segment flag (SPEC §5.1, §15) and any layout beyond the `Columns` section (SPEC §5.3.5) so a later writer can set the flag without breaking a v1.0 reader. ADR-0055 becomes accepted. This leaf writes no aggregation code
- [ ] 36.1.2 (v1.1) Columnar values for numeric and keyword fields per segment, behind the feature flag, with golden fixture. Aggregations read the `Columns` section (SPEC §5.3.5) and do not decode stored documents. Bytes that section does not already define are written only with the flag from 36.1.1. [T] A golden segment round-trips, and a segment that does not set the flag still opens

## 36.2 Metrics
- [ ] 36.2.1 (v1.1) count/sum/avg/min/max over hits on one snapshot, with exact numeric semantics (overflow, NaN, missing, empty) as ADR-0055. The snapshot and the hit set are the ones facets use (SPEC §7.5). [T] Empty, missing, non-finite, and saturating inputs match the ADR, including `saturated`
- [ ] 36.2.2 (v1.1) group-by on keyword field with nested metrics; deterministic ordering and bounded bucket count. Typed columns (`i64`, `f64`, `bool`, `datetime`) use the same rules (ADR-0055). `size` has the facet bounds (SPEC §7.5). [T] Order is the metric, then the key, and a bucket past `size` is absent
- [ ] 36.2.3 (v1.1) percentiles and histograms. Exact over the included values: nearest-rank percentiles, and histogram edges from the request (ADR-0055). [T] An empty set is null for a percentile and zero in every histogram bucket
- [ ] 36.2.4 (v1.1) Differential tests against a naive reference implementation over random corpora. The reference states ADR-0055 (saturation, missing values, empty sets, group-by order, percentiles, histograms). Seeded. The v1.2 expression and extractor leaves extend the same oracle

## 36.3 Expressions and extractors
- [ ] 36.3.1 (v1.2) computed field expressions (safe evaluator, bounded, no I/O) usable in aggregations and results. Arithmetic, comparisons, field refs, literals, `abs`, and `round`; no loops (ADR-0055). [T] Division by zero and a missing field are null, an expression over 64 nodes is rejected, and the same document always yields the same value
- [ ] 36.3.2 (v1.2) index-time extractors: regex capture and locale-aware number parsing into derived typed fields. `"1.500,50"` under `es` and `"1,500.50"` under `en` are the same value. Extraction runs at index time; a merge copies the column (ADR-0055). [T] The same input and locale always produce the same column, and a query does not parse the text again

## 36.4 API
- [ ] 36.4.1 (v1.2) CLI and embedded API surface for aggregations; SDK exposure tracked with M14-style follow-ups. Those follow-ups are new post-MVP leaves, not edits to M14. [T] The CLI and the embedded API return the engine's metrics for one snapshot

