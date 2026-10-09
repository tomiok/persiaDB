# Persia DB — Roadmap

How to read this file:
- Numbering is hierarchical: `M1` → `1.1` → `1.1.1` → `1.1.1.1`. **Every leaf is one GitHub issue** (title: `[1.1.1] short title`).
- A leaf is small: ≤ 1 day of focused work, one PR, independently testable.
- Non-leaf items group leaves (use GitHub milestones/labels or a tracking issue with task list).
- `(SPEC §x)` points to the section that defines the behavior. `[T]` marks the test expectation for that leaf.
- Dependencies are listed per milestone. Do not start a milestone before its dependencies are done unless noted.
- **Status** is tracked on leaves only: `[ ]` todo · `[~]` in progress · `[x]` done (DoD met). Parent status is derived.
  After changing a status run `python3 scripts/progress.py --record`; it regenerates [`PROGRESS.md`](./PROGRESS.md).
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
- [~] 0.2.1 PR workflow: fmt, clippy `-D warnings`, `cargo test --workspace`, on Linux + macOS; plus `cargo deny check` and `scripts/check_deps.py`
- [~] 0.2.2 Supply chain: `cargo deny` (licenses, advisories, bans incl. tantivy/rocksdb/sled), `cargo audit` scheduled
- 0.2.3 Scheduled workflows
  - [ ] 0.2.3.1 Nightly extended property tests (`--features slow`)
  - [ ] 0.2.3.2 Nightly fuzz run (time-boxed per target, corpus cached as artifact)
  - [ ] 0.2.3.3 Weekly benchmark run storing results (regression tracking)
- [ ] 0.2.4 Coverage report (`cargo llvm-cov`) uploaded as artifact, no gating yet
- [ ] 0.2.5 Miri job for `persia-format` pure-logic modules

## 0.3 Developer tooling
- [x] 0.3.1 `justfile` with `fmt`, `lint`, `test`, `fuzz`, `bench`, `integration`
- [ ] 0.3.2 Dev container / `docker-compose.dev.yml` skeleton (MinIO, fake-gcs-server, Azurite — filled in M12)
- [~] 0.3.3 Install Claude Code assets: `CLAUDE.md`, `.claude/agents/*`, `.claude/skills/*` committed and documented
- [ ] 0.3.4 GitHub issue/PR templates, labels, milestone setup script
- 0.3.5 Progress tracking
  - [x] 0.3.5.1 `scripts/progress.py`: ROADMAP checkboxes → `PROGRESS.md` (milestone graph, burn-up, ready-next list)
  - [~] 0.3.5.2 CI job: `python3 scripts/progress.py --check` fails on stale `PROGRESS.md`

## 0.4 Documentation scaffolding
- [x] 0.4.1 `docs/adr/0000-template.md` + ADR process
- [ ] 0.4.2 ADR-0001: record "own engine, no Tantivy" decision and allowed dependency list
- [ ] 0.4.3 ADR-0002: blob hash choice (SPEC §18.1)
- [ ] 0.4.4 `docs/architecture.md` (diagram from SPEC §3, glossary)

---

# M1 — Format primitives (`persia-format`)
Deps: M0. Pure functions, no I/O. This is the foundation: be paranoid.

## 1.1 Integer & byte encodings
- 1.1.1 Little-endian read/write helpers with bounds-checked cursor (`Reader`, `Writer`)
  - [ ] 1.1.1.1 `Reader` over `&[u8]` returning `Err(Corrupt)` on short reads, never panics
  - [ ] 1.1.1.2 `Writer` over `Vec<u8>` with alignment helpers (`pad_to(8)`)
  - [ ] 1.1.1.3 [T] property: write/read round-trip for all primitive widths; fuzz `Reader`
- 1.1.2 Varint (LEB128 u32/u64) encode/decode
  - [ ] 1.1.2.1 Encode/decode with overlong-encoding rejection
  - [ ] 1.1.2.2 [T] property round-trip; golden vectors; fuzz decode
- [ ] 1.1.3 Zigzag encoding for signed integers
- [ ] 1.1.4 Checked arithmetic helpers for offsets/lengths (`Offset`, `Len` newtypes)
- [ ] 1.1.5 `#![deny(clippy::arithmetic_side_effects)]` in `persia-format` (and later in engine segment readers); fix all hits with checked ops

## 1.2 Integer compression
- 1.2.1 Delta coding for sorted `u32` sequences
  - [ ] 1.2.1.1 Delta encode/decode with strictly-increasing validation
  - [ ] 1.2.1.2 [T] property round-trip for random sorted sets incl. empty, single, max values
- 1.2.2 Bit-packing of 128-value blocks (width 0..=32)
  - [ ] 1.2.2.1 Scalar pack/unpack for all widths
  - [ ] 1.2.2.2 Compute minimal bit-width for a block
  - [ ] 1.2.2.3 Packed-size formula + bounds checks on decode input
  - [ ] 1.2.2.4 [T] exhaustive width test; property round-trip; fuzz
  - [ ] 1.2.2.5 Benchmark scalar throughput (baseline for SIMD later)
- 1.2.3 Postings block codec: delta + bit-pack 128 docids, varint tail block
  - [ ] 1.2.3.1 Encode block with `last_docid`, `max_tf`, width metadata header
  - [ ] 1.2.3.2 Decode block into caller-provided buffer (no allocation in hot path)
  - [ ] 1.2.3.3 `advance_to(target)` within a block (galloping/binary search)
- [ ] 1.2.4 Frame-of-reference codec for numeric columns (i64/f64 via order-preserving bits)

## 1.3 Checksums & hashing
- [ ] 1.3.1 CRC32C wrapper (`crc32c` crate) with streaming API
- [ ] 1.3.2 xxh3-64/128 wrapper for content hashes and routing
- [ ] 1.3.3 `Checksummed<T>` helper for header structures; [T] bit-flip detection test (flip every bit in a sample, assert detected)

## 1.4 Compression
- [ ] 1.4.1 Codec enum (`None`, `Zstd`) + `compress_block`/`decompress_block`
- [ ] 1.4.2 Decompression-bomb guard: caller supplies max uncompressed size, reject mismatches
- [ ] 1.4.3 [T] fuzz decompress with random/mutated inputs

## 1.5 Block framing (SPEC §5.2)
- [ ] 1.5.1 `BlockHeader {crc32c, codec, uncompressed_len}` encode/decode
- [ ] 1.5.2 `BlockWriter`: split a byte stream into fixed-size blocks, emit block table
- [ ] 1.5.3 `BlockReader`: random-access decode of block *i*, verifying checksum
- [ ] 1.5.4 [T] property round-trip with random sizes; corrupted block detected and reported with block index

## 1.6 Container header & superblock (SPEC §4.2–4.3)
- 1.6.1 `Header` struct: magic, versions, flags, uuid, page size; encode/decode
  - [ ] 1.6.1.1 Reject unknown `format_major`, unknown `incompat_flags`; accept unknown `rocompat_flags` as read-only
  - [ ] 1.6.1.2 [T] golden header bytes; fuzz decode
- [ ] 1.6.2 `Superblock` slot encode/decode with CRC
- 1.6.3 Slot selection logic: pick highest valid `seq`; handle both invalid, one invalid, equal seq
  - [ ] 1.6.3.1 [T] truth-table unit tests + property test with random corruption

## 1.7 Frames (SPEC §4.4)
- [ ] 1.7.1 `FrameHeader`/`Frame` encode with trailing CRC and 8-byte padding
- 1.7.2 `FrameScanner<R>`: iterate frames from an offset, stop at first invalid frame, report reason
  - [ ] 1.7.2.1 Distinguish clean EOF vs torn tail vs mid-file corruption
  - [ ] 1.7.2.2 [T] property: truncate file at every byte offset → scanner yields exactly the complete prefix frames
  - [ ] 1.7.2.3 [T] fuzz scanner
- [ ] 1.7.3 Frame kinds registry (WalChunk, Segment, Manifest, Padding, TombstoneSidecar) with forward-compat skip of unknown kinds

## 1.8 Memory-mapped reader (the only `unsafe`)
- [ ] 1.8.1 `MmapFile` wrapper over `memmap2` with documented safety contract (file not truncated while mapped)
- [ ] 1.8.2 `Bytes`-like slice abstraction unifying mmap, heap and remote-cached buffers (`ByteSource`)
- [ ] 1.8.3 ADR documenting the unsafe boundary + SIGBUS strategy

## 1.9 Golden fixtures
- [ ] 1.9.1 Fixture generator binary producing deterministic sample containers
- [ ] 1.9.2 Check-in small fixtures (< 100 KB) + test that opens all fixtures

## 1.10 Shared value types
- [ ] 1.10.1 `BlobId` + `BlobRef { blob_id, size, mime, chunk_size, user_meta? }` with encode/decode (SPEC §9.1); hash algorithm per ADR-0002

---

# M2 — Storage abstraction & local backends (`persia-storage`)
Deps: M1.

## 2.1 Core trait and types (SPEC §10.1)
- [ ] 2.1.1 `Key`, `ObjectMeta`, `PutOutcome`, `Capabilities`, error taxonomy (`NotFound`, `Conflict`, `Transient`, `Corrupt`, `Io`)
- [ ] 2.1.2 Define `Storage` trait + `ByteSource` integration
- [ ] 2.1.3 Retry classification (`is_retryable`) and a generic retry policy helper (exp backoff + jitter, injectable clock)

## 2.2 In-memory backend
- [ ] 2.2.1 `MemoryStorage` implementing all ops incl. `put_if_absent` atomicity
- 2.2.2 Deterministic failure-injection wrapper `FaultyStorage`
  - [ ] 2.2.2.1 Inject: error on Nth call, latency, torn put (partial bytes), dropped put
  - [ ] 2.2.2.2 Seeded RNG schedule so failures are reproducible
- [ ] 2.2.3 [T] conformance test-suite (generic over `Storage`) reused by every backend

## 2.3 Local filesystem backend (object-per-file)
- [ ] 2.3.1 Atomic `put`: write temp → fsync → rename → fsync dir
- [ ] 2.3.2 `put_if_absent` via `O_EXCL`/`link` semantics
- [ ] 2.3.3 `get_range` using pread; `list` with prefix; `delete`
- [ ] 2.3.4 Path-safety: reject `..`, absolute keys, invalid UTF-8; [T] traversal attempts
- [ ] 2.3.5 Run conformance suite

## 2.4 Single-file container adapter (SPEC §4)
- 2.4.1 Open/create container: write header, init both superblock slots
  - [ ] 2.4.1.1 Exclusive writer lock (`flock`) with clear error when held
  - [ ] 2.4.1.2 Read-only open mode (also when rocompat flags unknown)
- [ ] 2.4.2 Append frames with group-fsync API (`append`, `sync`)
- 2.4.3 Commit: write inactive superblock slot, alternate (SPEC §4.5)
  - [ ] 2.4.3.1 [T] crash simulation: fail after each step 1/2/3, reopen, assert previous or new commit visible, never partial
- [ ] 2.4.4 Recovery on open: choose superblock, scan from `log_end`, truncate torn tail
- 2.4.5 Container compaction: rewrite live frames into new file + atomic rename (SPEC §4.6)
  - [ ] 2.4.5.1 Dead-space accounting
  - [ ] 2.4.5.2 [T] crash during compaction leaves a valid old or new file
- [ ] 2.4.6 Expose logical objects (Segment/Manifest/WalChunk) via the `Storage` trait view for the engine
- [ ] 2.4.7 Fuzz target: arbitrary bytes as container → open must return `Err` or valid, never panic
- [ ] 2.4.8 Platform notes: fsync semantics on macOS (`F_FULLFSYNC`), Windows (`FlushFileBuffers`), documented + tested where possible

---

# M3 — Schema, documents, collections (`persia-engine::schema`)
Deps: M1. (Parallel with M2.)

## 3.1 Schema model (SPEC §6)
- [ ] 3.1.1 Field types enum + per-field options (analyzer, boost, positions, k1, b, facet, stored)
- 3.1.2 `CollectionSchema` with validation (names, reserved words, limits, boost ranges)
  - [ ] 3.1.2.1 Schema JSON (de)serialization with strict unknown-key rejection
  - [ ] 3.1.2.2 Canonical encoding + schema hash (stable across versions)
- [ ] 3.1.3 Schema evolution rules: allow add field; reject type change/analyzer change; [T] table-driven tests
- [ ] 3.1.4 Ranking config (`static_rank`) and relaxation config structs with defaults

## 3.2 Document model
- [ ] 3.2.1 `Document { id, fields: Map<FieldId, Value>, blob_ref? }` and `Value` enum (`blob_ref` uses `persia_format::BlobRef`, 1.10.1)
- [ ] 3.2.2 Validation against schema (types, multi-valued fields, limits: doc size, field count, id length)
- [ ] 3.2.3 CBOR encode/decode for DocStore; deterministic field order
- [ ] 3.2.4 JSON ↔ Document conversion (strict/non-strict mode, SPEC §6.4)
- [ ] 3.2.5 [T] property: random schema + random valid docs round-trip; invalid docs rejected with precise errors

## 3.3 Manifest model
- [ ] 3.3.1 `Manifest { seq, fence_token, collections: {name → {schema, segments[], tombstone_versions}}, wal_start }`
- [ ] 3.3.2 Manifest encode/decode (versioned) + hash
- [ ] 3.3.3 Manifest diff/apply helpers for commit planning
- [ ] 3.3.4 [T] golden manifest fixtures; fuzz decode

---

# M4 — Text analysis (`persia-analysis`)
Deps: M0. (Parallel with M2/M3.)

## 4.1 Tokenizers
- 4.1.1 Unicode word segmentation (UAX#29) tokenizer with offsets
  - [ ] 4.1.1.1 Token struct `{text, start, end, position}`
  - [ ] 4.1.1.2 Handling of numbers, emails, URLs, apostrophes, CJK fallback (per-char)
  - [ ] 4.1.1.3 [T] table of tricky inputs; property: offsets valid and monotonic
- [ ] 4.1.2 Whitespace tokenizer, keyword tokenizer
- [ ] 4.1.3 Max token length guard (drop/truncate, configurable)

## 4.2 Normalization & filters
- [ ] 4.2.1 Unicode NFKC normalization filter
- [ ] 4.2.2 Lowercasing (Unicode-aware, including `ß`, Turkish-i policy documented)
- [ ] 4.2.3 Accent/diacritic folding (`ñ`→`n` optional per analyzer, `ü`→`u`)
- 4.2.4 Stopword filter with embedded lists
  - [ ] 4.2.4.1 English list
  - [ ] 4.2.4.2 Spanish list
  - [ ] 4.2.4.3 Portuguese list
- [ ] 4.2.5 Synonym filter (expansion at query time; config from schema) (SPEC §7.6 L1)
- [ ] 4.2.6 Edge n-gram filter (for optional prefix indexing)

## 4.3 Stemmers
- [ ] 4.3.1 Stemmer trait + Snowball-style English (Porter2)
- [ ] 4.3.2 Spanish stemmer
- [ ] 4.3.3 Portuguese stemmer
- [ ] 4.3.4 [T] golden word lists per language; property: stemming is idempotent where documented

## 4.4 Analyzer pipelines
- [ ] 4.4.1 `Analyzer` = char filters → tokenizer → token filters; builder + registry by name+version
- [ ] 4.4.2 Built-ins: `standard`, `keyword`, `whitespace`, `en`, `es`, `pt`
- [ ] 4.4.3 Analyzer identity/versioning recorded in schema (SPEC §6.3)
- [ ] 4.4.4 Zero-allocation fast path (reuse buffers via `TokenStream`)
- [ ] 4.4.5 Benchmark: tokens/sec per analyzer; fuzz: arbitrary UTF-8 never panics

---

# M5 — Segment writer & reader (`persia-engine::segment`)
Deps: M1, M3, M4. The heart of the index format.

## 5.1 In-memory index builder
- 5.1.1 Term → postings accumulator per field (docid, tf, positions)
  - [ ] 5.1.1.1 Term interning/arena to avoid per-term allocation
  - [ ] 5.1.1.2 Per-doc field length accounting (norms input)
- [ ] 5.1.2 Column builders (keyword dict + ords, numeric, bool) per field
- [ ] 5.1.3 DocStore builder (CBOR rows → zstd blocks)
- [ ] 5.1.4 IdMap builder (id → docid, docid → id)
- [ ] 5.1.5 Builder memory accounting (`size_in_bytes`) for flush thresholds

## 5.2 Section writers (SPEC §5.3)
- 5.2.1 Term dictionary (FST) writer with term-info array
  - [ ] 5.2.1.1 Sorted term emission; value = index into term-info
  - [ ] 5.2.1.2 Term-info record encoding (postings off, df, ttf, skip off, positions off)
- [ ] 5.2.2 Postings + freqs writer (128-blocks) with skip entries (`last_docid`, offset, `max_tf`, `min_norm`)
- [ ] 5.2.3 Positions writer (opt-in per field)
- [ ] 5.2.4 Norms writer (1-byte quantization table; document quantization error bounds)
- [ ] 5.2.5 Columns writer (dictionary-encoded keyword, FOR numeric, roaring sparse)
- [ ] 5.2.6 DocStore, IdMap, BlobRefs section writers
- [ ] 5.2.7 Segment footer writer: stats, section table, schema hash, CRC; trailing locator (SPEC §5.1)
- [ ] 5.2.8 `SegmentWriter::finish() -> Bytes/Writer` orchestrating all sections with deterministic output (same input → same bytes)

## 5.3 Segment reader
- [ ] 5.3.1 Open: locate/validate footer, parse section table, verify schema hash and version
- [ ] 5.3.2 Term lookup: FST get + term-info decode
- 5.3.3 Postings iterator
  - [ ] 5.3.3.1 Sequential `next()` over blocks
  - [ ] 5.3.3.2 `advance(target)` using skip entries
  - [ ] 5.3.3.3 Frequency access aligned with docid blocks
  - [ ] 5.3.3.4 Block-level metadata access (`max_tf`, `last_docid`) for block-max WAND
- [ ] 5.3.4 Positions iterator (phrase support)
- [ ] 5.3.5 Norms accessor, columns accessor (typed readers)
- [ ] 5.3.6 Doc fetch by docid (DocStore block cache) and by external id (IdMap FST)
- [ ] 5.3.7 Tombstone overlay view (roaring) applied on iteration
- [ ] 5.3.8 Prefix/range term enumeration (FST range) and Levenshtein-automaton intersection hook (used in M8)

## 5.4 Verification & tooling
- [ ] 5.4.1 `Segment::verify()`: recompute all block CRCs, cross-check stats vs data
- [ ] 5.4.2 `persia inspect segment` (CLI): footer, sections, sizes, per-field stats
- [ ] 5.4.3 `persia dump segment --field f --term t` debugging output

## 5.5 Tests
- [ ] 5.5.1 Property: build segment from random docs → read back every doc/term/posting/column exactly equals the model
- [ ] 5.5.2 Golden segment fixtures (+ determinism test)
- [ ] 5.5.3 Fuzz targets: footer parser, each section decoder, term dict, postings block decoder, full-segment open
- [ ] 5.5.4 Large-doc / empty-collection / single-doc / 2^20-doc edge-case tests
- [ ] 5.5.5 Bench: build throughput, term lookup, postings decode, doc fetch

---

# M6 — Write path, durability, merging (`persia-engine`)
Deps: M2, M5.

## 6.1 Memtable
- [ ] 6.1.1 Mutable in-memory index structure (concurrent reads, single writer)
- [ ] 6.1.2 Searchable view implementing the same iterator traits as a segment
- [ ] 6.1.3 Upsert/delete semantics inside memtable (replace within same buffer)
- [ ] 6.1.4 Size/age flush triggers

## 6.2 WAL (SPEC §8.2)
- [ ] 6.2.1 WAL record format: op (put/delete), collection id, payload, checksum
- [ ] 6.2.2 Group commit: batch appends, single fsync, durability levels (`Buffered|Fsync|Async`)
- [ ] 6.2.3 WAL chunk rotation and `wal_start` advancement
- 6.2.4 Replay: validate CRCs, stop at first invalid, idempotent
  - [ ] 6.2.4.1 [T] truncate WAL at every byte → recovered state equals a prefix of operations
- [ ] 6.2.5 Fuzz WAL replay

## 6.3 Flush & commit
- [ ] 6.3.1 Memtable → segment (uses M5 writer) → storage frame/object
- [ ] 6.3.2 Manifest build + commit protocol (local, SPEC §4.5)
- [ ] 6.3.3 Atomic snapshot publication to readers (`ArcSwap`-style) with `seq`
- [ ] 6.3.4 Auto-commit policy (size, interval, explicit `commit()`)
- [ ] 6.3.5 [T] crash tests at every protocol step using `FaultyStorage`

## 6.4 Upserts & deletes across segments
- [ ] 6.4.1 Lookup docid by external id newest→oldest segment (IdMap FST)
- [ ] 6.4.2 Tombstone sidecar format + versioning (SPEC §8.3, ADR)
- [ ] 6.4.3 Delete-by-id and upsert through commit; visibility rules (read-your-writes)
- [ ] 6.4.4 [T] differential tests vs. reference model for random put/delete/commit sequences

## 6.5 Compaction / merging (SPEC §8.5)
- [ ] 6.5.1 Merge policy (tiered) as pure function: segments → merge plans; [T] table tests
- 6.5.2 Segment merger: N readers → one writer, dropping tombstoned docs, remapping docids
  - [ ] 6.5.2.1 Merge term dictionaries and postings (k-way)
  - [ ] 6.5.2.2 Merge columns, norms, doc store, id map, blob refs
  - [ ] 6.5.2.3 Recompute statistics
- [ ] 6.5.3 Background merge scheduler (thread pool, cancellation, backpressure)
- [ ] 6.5.4 Safe deletion of obsolete segments via snapshot epoch/refcount
- [ ] 6.5.5 [T] property: merge(search results) == pre-merge results (same hits/scores within epsilon)

## 6.6 Open/recovery orchestration
- [ ] 6.6.1 Open sequence: superblock → manifest → replay WAL → memtable → ready (SPEC §8.6)
- [ ] 6.6.2 Single-writer enforcement + read-only open for readers
- [ ] 6.6.3 `persia verify` full database verification command
- [ ] 6.6.4 Differential + crash-loop test harness (kill -9 child process in loop, reopen, check invariants) — shared with integration-tester

---

# M7 — Query engine & ranking (`persia-engine::query`)
Deps: M5 (M6 for end-to-end).

## 7.1 Query AST & parser
- [ ] 7.1.1 AST: Term, Phrase, Prefix, Fuzzy, Bool{must,should,must_not,filter}, Boost, ConstantScore, MatchAll
- 7.1.2 User text → AST compiler (analysis per field, default AND, quoted phrases, `-` exclusion, `field:`)
  - [ ] 7.1.2.1 Never fail on user input (invalid syntax degrades to plain terms)
  - [ ] 7.1.2.2 [T] fuzz + table-driven tests
- [ ] 7.1.3 Filter DSL (term, terms, range, exists, and/or/not) parser + validation against schema
- [ ] 7.1.4 Query limits (max terms/clauses/expansions) with explicit errors

## 7.2 Collection statistics
- [ ] 7.2.1 Aggregate `N`, `avglen`, `df` across live segments (cached per snapshot)
- [ ] 7.2.2 Statistics provider trait (enables global stats in M13)

## 7.3 BM25F scorer (SPEC §7.2)
- [ ] 7.3.1 Scorer implementation with per-field `k1`, `b`, weights; norm decode table
- [ ] 7.3.2 Static rank contribution (log1p, linear, saturation functions)
- [ ] 7.3.3 Score upper bounds per term/block (for WAND)
- [ ] 7.3.4 [T] unit tests vs hand-computed examples; differential vs reference scorer within epsilon

## 7.4 Matching & top-k
- [ ] 7.4.1 Single-term, AND, OR, NOT iterators with skip
- [ ] 7.4.2 Phrase matching using positions
- [ ] 7.4.3 Top-k collector (bounded heap, tie-break determinism: score desc, then docid)
- 7.4.4 MaxScore/WAND, then block-max WAND
  - [ ] 7.4.4.1 MaxScore baseline
  - [ ] 7.4.4.2 Block-max WAND using skip metadata
  - [ ] 7.4.4.3 [T] differential: pruned results == exhaustive results
- [ ] 7.4.5 Filter evaluation to roaring bitmaps/column predicates; intersect with candidates
- [ ] 7.4.6 Tombstone application; memtable as additional segment

## 7.5 Sorting, pagination, facets
- [ ] 7.5.1 Sort by column (asc/desc, missing handling) with top-k
- [ ] 7.5.2 `offset/limit` and `search_after` cursor (SPEC §7.4)
- [ ] 7.5.3 Facet counting over keyword columns with bitmap input; top-N merge across segments
- [ ] 7.5.4 Result assembly: fetch stored fields, projection, blob_ref in hit

## 7.6 Parallel execution
- [ ] 7.6.1 Per-segment search tasks on a thread pool; deterministic merge
- [ ] 7.6.2 Query deadline/cancellation token

## 7.7 Explain
- [ ] 7.7.1 Per-hit score breakdown (per term/field) when `explain: true`

## 7.8 Tests & benchmarks
- [ ] 7.8.1 Differential suite vs. reference engine (random corpora/queries)
- [ ] 7.8.2 Relevance sanity corpus with judged queries; nDCG@10 baseline recorded
- [ ] 7.8.3 Criterion benchmarks: single term, AND/OR, phrase, filtered, sorted, facets

---

# M8 — Zero-results avoidance (`persia-engine::relax`)
Deps: M7. **Key differentiator — treat quality here as a product feature.**

## 8.1 Framework
- [ ] 8.1.1 `RelaxPlan` = ordered levels with per-level query transforms; configurable via schema/query (SPEC §7.6)
- [ ] 8.1.2 Orchestrator: run L0, evaluate `min_results`, escalate, stop or fill
- [ ] 8.1.3 Score offset per level to guarantee ordering across levels
- [ ] 8.1.4 Budget (`budget_ms`) and `truncated` flag
- [ ] 8.1.5 Response metadata struct (`level`, `applied`, `original`, `effective`, `corrected`)

## 8.2 Levels
- [ ] 8.2.1 L1 synonyms: expand terms with synonym groups
- 8.2.2 L2 drop_terms
  - [ ] 8.2.2.1 Term importance ordering (IDF + position + stopword flag)
  - [ ] 8.2.2.2 `minimum_should_match` ladder 100/75/50/1
  - [ ] 8.2.2.3 Single-term queries skip L2
- [ ] 8.2.3 L3 prefix expansion (FST range, cap expansions, rank by df)
- 8.2.4 L4 fuzzy
  - [ ] 8.2.4.1 Levenshtein automaton builder (distance 1–2, transpositions)
  - [ ] 8.2.4.2 FST ∩ automaton term enumeration with cap + df ranking
  - [ ] 8.2.4.3 "Did you mean" corrected-query synthesis
  - [ ] 8.2.4.4 Length-based distance policy (SPEC §7.6 L4)
- [ ] 8.2.5 L5 fallback: static_rank / recency top-N; clearly flagged in response
- [ ] 8.2.6 Optional filter relaxation (`last_resort`) with explicit opt-in

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
- [ ] 11.1.3 Rust codegen crate `persia-proto`

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

## 12.4 Block cache (SPEC §10.6)
- [ ] 12.4.1 RAM cache (sharded LRU/ARC) keyed by `(object, block)`
- [ ] 12.4.2 NVMe disk cache tier with crash-safe index (or rebuild-on-start)
- [ ] 12.4.3 Range coalescing and readahead for sequential sections
- [ ] 12.4.4 Singleflight (dedup concurrent identical fetches)
- [ ] 12.4.5 Pinning of segment footers/FSTs; cache metrics
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
