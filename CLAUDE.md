# CLAUDE.md — Persia DB

Persia DB is an **embedded, single-file, cloud-native search database** written in Rust.
Think "SQLite for search": open a file, index documents, get ranked results that
(by default) never come back empty. The same on-disk format can be served from
object storage (S3 / GCS / Azure) and scaled horizontally on Kubernetes.

The authoritative design lives in [`SPEC.md`](./SPEC.md). Work is planned in
[`ROADMAP.md`](./ROADMAP.md); every leaf item there maps to one GitHub issue.
Current status is in [`PROGRESS.md`](./PROGRESS.md) (generated from ROADMAP checkboxes — **read it at the start of every task**).
**If code and SPEC disagree, stop and fix the SPEC first (or ask), never silently diverge.**

## Product pillars (do not trade these away)

1. **Zero friction**: `Db::open("app.persia")` just works. No daemon, no config files.
2. **Own engine**: the search engine (index format, analysis, scoring, query) is written
   from scratch in this repo. No Tantivy, Lucene bindings, SQLite, RocksDB, or any other
   search/storage engine as a dependency.
3. **Never zero results by default**: query relaxation is a core feature, not an add-on.
4. **Cloud-native**: immutable segments, object-storage friendly, stateless query nodes.
5. **Correctness over speed, then speed**: durability and crash safety are non-negotiable;
   performance claims must be backed by reproducible benchmarks.

## Repository layout

```
crates/
  persia-format/    # on-disk primitives: encodings, frames, superblock, checksums (no I/O policy)
  persia-storage/   # Storage trait + backends (memory, local file, s3, gcs, azure)
  persia-analysis/  # tokenizers, normalizers, stemmers, analyzers
  persia-engine/    # segments, indexing, WAL, compaction, query, scoring, relaxation
  persia-blob/      # content-addressed blob store (chunks, dedup, GC)
  persia/           # public embedded API (Db, Collection) — the facade users import
  persia-proto/     # .proto definitions + generated code
  persia-server/    # gRPC + HTTP server binary (Docker image)
  persia-cli/       # `persia` CLI: inspect, dump, verify, compact, bench
  persia-testutil/  # dev-only: shared proptest strategies, synthetic generators, test helpers (never a normal dependency)
sdk/
  rust/             # persia-client (network client)
  go/               # persia-go (network client)
tests/
  integration/      # cross-crate + docker-compose based tests
  synthetic/        # data generators, load & chaos scenarios
benches/            # criterion + end-to-end benchmark harness
fuzz/               # cargo-fuzz targets
docs/adr/           # architecture decision records
scripts/            # repo tooling (progress.py)
.claude/agents/     # subagents: test-writer, code-reviewer, integration-tester
.claude/skills/     # skills: implement-issue
PROGRESS.md         # GENERATED status graph — never edit by hand
```

## Commands

`just check` runs everything CI runs; `just` lists all recipes. The underlying commands:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace                       # unit + property tests (must be fast, < 2 min)
cargo test --workspace --features slow       # extended property tests (each crate has `slow = []`; gate with #[cfg(feature = "slow")])
cargo nextest run --workspace                # preferred runner if installed
cargo deny check                             # licenses + advisories
just fuzz <target> [seconds]                 # dated nightly from .github/nightly-toolchain
cargo bench -p persia-engine
just miri                                    # Miri on persia-format (dated nightly)
just integration                             # docker-compose: MinIO, fake-gcs, azurite
python3 scripts/progress.py --record         # regenerate PROGRESS.md + append today's snapshot
python3 scripts/progress.py --check          # CI: fail if PROGRESS.md is stale
python3 scripts/check_deps.py                # internal crate dependency direction
python3 -m unittest discover -s scripts      # tests for repo scripts
```

Always run fmt, clippy and tests before declaring a task done.

## Toolchain

- Pinned in `rust-toolchain.toml`; rustup picks it up automatically.
- MSRV policy: until v1.0, MSRV equals the pinned toolchain (`rust-version` in `Cargo.toml`), so the MSRV is always
  the tested version. Bump both together in one `chore(toolchain)` PR (`scripts/test_toolchain.py` enforces this).
  Write an ADR before the first crates.io publish (i.e. before any `publish = false` is flipped), e.g. support the last
  N stable releases with a CI job on the MSRV.
- Bump cadence: within ~6 weeks of each stable release, as a standalone PR that also fixes any new clippy lints.
- Nightly-only tools (cargo-fuzz, Miri) use the **dated** nightly in `.github/nightly-toolchain` (CI and `just`
  read it from there), so they stay reproducible too. Bump it deliberately, like the stable pin.

## Coding rules

**Safety & errors**
- `#![forbid(unsafe_code)]` in every crate **except** `persia-format::mmap` (and any module
  explicitly listed in an ADR). Every `unsafe` block needs a `// SAFETY:` comment stating the invariant.
- Library crates never `panic!`, `unwrap()`, `expect()` or index out of bounds on data that
  comes from disk, network, or users. **Corrupted input must produce `Err(Error::Corrupt{..})`**, never UB or a panic.
- Errors: `thiserror` in libraries, `anyhow` only in binaries and tests.
- Integer arithmetic on sizes/offsets uses checked ops (`checked_add`, `try_from`).

**Format discipline**
- Anything written to disk is little-endian, explicitly sized (`u32`, `u64`), and covered by a checksum.
- Any change to on-disk layout requires: (1) SPEC.md update, (2) format version bump or feature flag,
  (3) an ADR in `docs/adr/`, (4) a golden-file test. No exceptions.
- Segments are **immutable** once written. Never mutate a published segment.
- Write order is part of correctness: data first, commit record last, then fsync/ack (see SPEC §8).

**Architecture**
- `persia-engine` is **synchronous** and storage-agnostic; it talks to I/O only through the `Storage` trait.
  `async` lives in `persia-storage` cloud backends and `persia-server`.
- Dependencies point downward. No cycles, no upward edges (enforced by `scripts/check_deps.py`; its `ALLOWED` table is the source of truth):
  - `persia-format` ← `persia-storage` ← `persia-engine` ← `persia` ← `persia-server` ← SDKs (`persia-proto` sits beside `persia-server`).
  - `persia-analysis` has no internal dependencies; it is used by `persia-engine`.
  - `persia-blob` depends on `persia-format` + `persia-storage`; it is used by `persia` (not by the engine).
    Shared value types (`BlobRef`, `BlobId`) live in `persia-format` so the engine can store them.
  - `persia-testutil` is a `[dev-dependencies]` entry only.
- Public APIs are small, documented (`#![deny(missing_docs)]` on public crates), and have doctests.

**Dependencies**
- Allowed building blocks (libraries): `fst`, `roaring`, `zstd`, `crc32c`, `xxhash-rust`, `memmap2`, `ciborium`, `sha2` (blob ids, ADR-0002),
  `serde`, `serde_json`, `thiserror`, `tracing`, `bytes`, `arc-swap`, `rayon`, `object_store`, `tokio`, `tonic`, `prost`, `axum`,
  `unicode-segmentation` (UAX#29), `unicode-normalization` (NFKC) — Unicode data tables are not worth hand-maintaining.
- Binaries (`persia-server`, `persia-cli`) may additionally use: `clap`, `anyhow`, `rustls`, `tracing-subscriber`,
  `opentelemetry*`, a Prometheus exporter.
- Dev/bench only: `proptest`, `criterion`, `insta`, `tempfile`, `testcontainers`, `hdrhistogram`; `libfuzzer-sys` in `fuzz/` only.
- The allowed list lives in root `[workspace.dependencies]`; `scripts/check_deps.py` rejects any direct dependency not
  declared there, and `deny.toml` bans engines and checks licenses/advisories for the whole graph. See ADR-0001.
- Anything else: justify in the PR. Banned: `tantivy`, `rusqlite`, `rocksdb`, `sled`, any search/KV engine.
- Prefer writing the 100-line primitive (varint, bitpacking, delta coding) over adding a crate.
- **No `build.rs`.** Generated code (e.g. gRPC stubs for `persia-proto`) is produced by a script, committed, and
  CI fails if regenerating it changes anything. Users must never need `protoc` or other tools to build.
  Data tables use `const fn` or `include_bytes!`. Enforced by `scripts/check_deps.py`.

**Style**
- `rustfmt` defaults, clippy pedantic where practical. Small functions, small modules, no `mod.rs` sprawl.
- Names: `snake_case`, domain terms from SPEC §2 (Segment, Collection, Manifest, Frame, Blob...). Do not invent synonyms.
- Comments explain *why*, not *what*. Link SPEC sections (`// SPEC §5.3`) in format code.
- All docs, comments, commit messages, issues and identifiers are **in English**.

## Testing rules (summary — see `test-writer` and `integration-tester` agents)

- Every item ships with tests in the same PR. Bug fix = failing test first.
- Layers: unit → property (`proptest`) → golden files (`insta`/binary fixtures) → fuzz → integration → synthetic/chaos.
- Anything that parses bytes needs a fuzz target. Anything with a round-trip needs a property test.
- The engine is tested **differentially** against a naive in-memory reference implementation (`persia-engine/tests/reference`).
- Tests are deterministic: seed RNGs, print the seed on failure, no wall-clock sleeps (use injected clocks).

## Workflow

1. Pick one GitHub issue (maps to one ROADMAP leaf). Read the linked SPEC sections first.
2. Use the `implement-issue` skill: plan → implement → tests → review.
3. Before finishing, delegate in this order:
   - `test-writer` — add/strengthen unit, property, golden, fuzz tests for the change.
   - `code-reviewer` — independent review of the diff against SPEC and these rules.
   - `integration-tester` — only for items touching storage, durability, server, SDKs, distribution.
4. One issue = one PR = one focused change. Conventional commits (`feat(engine): ...`, `fix(format): ...`).
5. Update `SPEC.md` / ADRs in the same PR when a decision changes.
6. Progress: mark the leaf `[~]` in `ROADMAP.md` when starting, `[x]` when the DoD is met, then run
   `python3 scripts/progress.py --record` and commit `ROADMAP.md`, `PROGRESS.md` and `docs/progress-history.csv` with the change.
   Never tick a leaf whose DoD is not met; add newly discovered work as new ROADMAP leaves instead of hiding it.

## Definition of Done (every issue)

- [ ] Behavior matches SPEC (cite section) or SPEC updated in the same PR
- [ ] fmt, clippy (`-D warnings`), tests, and `cargo deny` pass
- [ ] New tests at the right layers; fuzz target if bytes are parsed
- [ ] No new `unsafe` without `SAFETY` + ADR; no `unwrap` on external data
- [ ] Public API documented; CHANGELOG entry if user-visible
- [ ] Benchmarks added/updated if on a hot path (query, indexing, block decode)
- [ ] Reviewed by `code-reviewer` with no unresolved blockers
- [ ] ROADMAP leaf ticked and `PROGRESS.md` regenerated (`scripts/progress.py --check` passes)

## Things Claude must never do

- Add a search/KV engine dependency, or "temporarily" shell out to one.
- Change the on-disk format without the 4-step format discipline above.
- Skip or weaken a test to make CI green; delete failing tests; add `#[ignore]` without an issue link.
- Swallow errors (`let _ =`) on I/O paths, or fsync "later".
- Invent behavior not in SPEC: ask or propose a SPEC change instead.
- Commit secrets, real credentials, or large binary fixtures (> 1 MB; generate them instead).
