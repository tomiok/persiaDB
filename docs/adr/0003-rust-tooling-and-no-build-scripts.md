# ADR-0003: Repo tooling in Rust (`xtask`), and no build scripts

- Status: accepted
- Date: 2026-10-09
- Related: ADR-0001 (amends its enforcement details), CLAUDE.md "Dependencies"; ROADMAP 0.5, 11.1.3–11.1.4

## Context

Repo tooling (dependency-rule checks, progress graph, GitHub setup) started as Python scripts. That added a second
runtime for contributors and CI, and the project owner asked to avoid Python. Separately, gRPC code generation is
usually done in a `build.rs` with `prost-build`/`tonic-build`, which needs a `protoc` binary at build time. Every
user would then need `protoc` to compile Persia, which breaks the zero-friction pillar, and build scripts run
arbitrary code on every build.

## Decision

1. Repo tooling lives in the `xtask` workspace crate, run as `cargo xtask <command>` (alias in `.cargo/config.toml`).
   The pinned Rust toolchain is the only requirement. No Python or other runtimes in the repo.
2. A fourth dependency tier, **repo tooling only**: `toml`, allowed only in `xtask`. `xtask` is a binary, so it may
   also use the binary-only crates (`anyhow`, ...). Enforced by `cargo xtask check-deps` (`TOOLING_ONLY`, `BINARIES`).
   This replaces ADR-0001's references to `scripts/check_deps.py`.
3. **No `build.rs` anywhere** (also enforced by `cargo xtask check-deps`). Generated code, such as the gRPC stubs in
   `persia-proto`, is produced by an xtask command, committed, and CI regenerates it and fails on any diff
   (ROADMAP 11.1.4). Data tables use `const fn` or `include_bytes!`.

## Consequences

- One language and one toolchain for code and tooling; tooling is tested with `cargo test` like everything else.
- Building Persia never needs `protoc` or any other external tool.
- Generated code is reviewed in PRs and versioned with the `.proto` files.
- Regenerating proto code is an explicit step (`cargo xtask proto`), and CI catches anyone who forgets it.

## Alternatives considered

- **Keep Python tooling:** working and already tested, but a second runtime the owner did not want.
- **Bash scripts:** fine for small glue, fragile for the ROADMAP parser and graph renderer.
- **`build.rs` codegen with a vendored `protoc`:** avoids the install step, but still runs codegen on every build
  and hides the generated API from review.
