---
title: "Contributing"
description: "How to pick a roadmap leaf, what a pull request has to pass, and where to report security issues."
head:
  - tag: title
    content: "How to contribute to the Rust search database | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "How to contribute to the Rust search database | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "How to contribute to the Rust search database | PersiaDB"
---

Persia DB is a database, so correctness comes first and speed comes after, with measurements.

1. Read [`SPEC.md`](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) and [`CLAUDE.md`](https://github.com/tomiok/persiaDB/blob/main/CLAUDE.md). The rules apply to human and
   AI-assisted changes.
2. Pick a leaf from [`ROADMAP.md`](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md). [`PROGRESS.md`](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) lists what is
   ready next. One leaf, one pull request.
3. If the SPEC is unclear, or you want to change a decision, open an issue before writing the code.
   The code and the SPEC are not allowed to diverge silently.

Setup is the pinned toolchain (rustup reads `rust-toolchain.toml`) and `cargo test --workspace`.
Before a pull request: format, clippy with warnings denied, tests, `cargo xtask check-deps`, and
`cargo xtask progress --check`. The long form is [`CONTRIBUTING.md`](https://github.com/tomiok/persiaDB/blob/main/CONTRIBUTING.md).

On-disk format changes need all four of: a SPEC update, a version or flag bump, an ADR under
[`docs/adr/`](https://github.com/tomiok/persiaDB/blob/main/docs/adr/), and a golden test. Bytes from disk, the network, or a user are never
`unwrap`ped; corruption is `Err`. `unsafe` is confined to the memory-map module in `persia-format`,
and any other use needs an ADR.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
`feat(engine): ...`, `fix(format): ...`, `docs: ...`.

By participating you agree to the [Code of Conduct](https://github.com/tomiok/persiaDB/blob/main/CODE_OF_CONDUCT.md).
Report security issues privately, as described in [`SECURITY.md`](https://github.com/tomiok/persiaDB/blob/main/SECURITY.md). The project is
pre-alpha: only the latest commit on `main` receives fixes. Treat a panic or unbounded memory use
on a crafted database file as a security issue (SPEC §17).
