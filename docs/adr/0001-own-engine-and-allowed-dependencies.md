# ADR-0001: Own search engine, and a closed list of allowed dependencies

- Status: accepted (enforcement details amended by ADR-0003)
- Date: 2026-10-09
- Related: CLAUDE.md "Product pillars" (2) and "Dependencies"; SPEC §1, §5, §17; ROADMAP 0.4.2

## Context

Persia DB's value is a search engine designed around one single-file, immutable-segment format that is
served unchanged from object storage (SPEC §3, §5). That format, the analysis pipeline, BM25F scoring and the
zero-results relaxation cascade (SPEC §7.6) are the product. Embedding an existing engine (Tantivy, Lucene via
bindings) or storage engine (SQLite, RocksDB, sled, LMDB) would put the format and its crash-safety guarantees
in someone else's hands and make SPEC §4–§5 unimplementable as written.

At the same time, some building blocks are not where the value is: FSTs, roaring bitmaps, zstd, CRC32C, xxh3,
Unicode tables, the async/gRPC/HTTP stack and object-store clients.

## Decision

1. The index format, analysis, scoring, query execution, WAL, compaction and commit protocol are written in
   this repository. No search, SQL or key-value engine may be a dependency, directly or transitively, and
   none may be invoked as a subprocess.
2. External crates are a closed list: the root `[workspace.dependencies]` table, as listed in CLAUDE.md
   "Dependencies". The tiers are:
   - libraries: `fst`, `roaring`, `zstd`, `crc32c`, `xxhash-rust`, `memmap2`, `ciborium`, `serde`, `serde_json`,
     `thiserror`, `tracing`, `bytes`, `arc-swap`, `rayon`, `object_store`, `tokio`, `tonic`, `prost`, `axum`,
     `unicode-segmentation`, `unicode-normalization`, and `sha2` (ADR-0002);
   - binaries only (`persia-server`, `persia-cli`): `clap`, `anyhow`, `rustls`, `tracing-subscriber`,
     plus OpenTelemetry and Prometheus exporters when M11 needs them;
   - dev/bench only: `proptest`, `criterion`, `insta`, `tempfile`, `testcontainers`, `hdrhistogram`.
3. Enforcement is mechanical, not by review alone:
   - `scripts/check_deps.py` rejects any direct dependency missing from the table, any tier violation, and any
     dependency not inherited with `.workspace = true`;
   - `deny.toml` bans known engines in the full graph and checks licenses, advisories and sources.
4. Adding a crate requires a PR that justifies it and updates the table, CLAUDE.md, the tiers in
   `scripts/check_deps.py` (or a new ADR for anything significant). The list in item 2 is a snapshot taken when
   this ADR was accepted; the living list is `[workspace.dependencies]` plus CLAUDE.md.

## Consequences

- More code to write and test: varint, bit-packing, block codecs, BM25F, WAND, the relaxation cascade.
  The testing rules (property, golden, fuzz, differential against a reference model) exist to make that safe.
- We control the on-disk format end to end, which is what makes object-storage serving and the crash-safety
  guarantees in SPEC §4.5 possible.
- A small, audited dependency graph: fewer advisories, faster builds, easier supply-chain review (SPEC §17).
- Unicode segmentation and normalization are delegated, because their data tables track Unicode releases and
  are not worth maintaining by hand.

## Alternatives considered

- **Build on Tantivy.** Fastest path to working search, but its segment format, merge policy and directory
  abstraction would dictate ours, and zero-results relaxation would be layered on top instead of designed in.
- **SQLite FTS5 or RocksDB as the storage layer.** Mature and crash-safe, but not object-storage native, and it
  defeats the "own engine" pillar.
- **An open dependency policy with review only.** Cheaper day to day, but transitive engines and license
  problems slip in unnoticed; the mechanical checks cost little.
