# Contributing to Persia DB

Thanks for your interest! Persia DB is a database, so the bar is correctness first, then speed.

## Before you start

1. Read [`SPEC.md`](./SPEC.md) (the design) and [`CLAUDE.md`](./CLAUDE.md) (the engineering rules; they apply to
   humans too).
2. Pick a leaf from [`ROADMAP.md`](./ROADMAP.md); [`PROGRESS.md`](./PROGRESS.md) lists what is ready next.
   Each leaf is one issue and one PR.
3. If the SPEC is unclear or you want to change a decision, open an issue first. Code and SPEC must never disagree.

## Setup

```bash
# rustup reads rust-toolchain.toml and installs the pinned toolchain automatically
git clone https://github.com/tomiok/persiaDB.git && cd persiaDB
cargo test --workspace
```

## Before opening a PR

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
python3 scripts/check_deps.py
python3 -m unittest discover -s scripts
python3 scripts/progress.py --check
```

- Tests ship with the change, at the right layers (unit, property, golden, fuzz; see `CLAUDE.md`).
  A bug fix starts with a failing test.
- On-disk format changes need a SPEC update, a version or flag bump, an ADR in `docs/adr/`, and a golden test.
- Tick the ROADMAP leaf (`[x]`) only when the Definition of Done in `CLAUDE.md` is met, then run
  `python3 scripts/progress.py --record`.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/): `feat(engine): ...`,
  `fix(format): ...`, `docs: ...`.
- New dependencies must be on the allowed list in `CLAUDE.md`, or be justified in the PR.

## Working with Claude Code

The repo ships its own [Claude Code](https://claude.com/claude-code) setup, so AI-assisted work follows the
same rules as everyone else:

| Asset | What it does |
|---|---|
| [`CLAUDE.md`](./CLAUDE.md) | Project rules loaded into every session (pillars, coding and testing rules, Definition of Done) |
| [`.claude/skills/implement-issue`](./.claude/skills/implement-issue/SKILL.md) | `/implement-issue <leaf>`: plan from SPEC → implement → test → review → tick ROADMAP |
| [`.claude/agents/test-writer.md`](./.claude/agents/test-writer.md) | Adds unit, property, golden, fuzz, differential and crash tests |
| [`.claude/agents/code-reviewer.md`](./.claude/agents/code-reviewer.md) | Read-only review of a diff against SPEC and CLAUDE.md |
| [`.claude/agents/integration-tester.md`](./.claude/agents/integration-tester.md) | System tests: crash loops, cloud emulators, load, relevance |

Personal settings go in `.claude/settings.local.json` (git-ignored). Claude-authored commits carry a
`Co-Authored-By` trailer and get the same human review as any other PR.

## Conduct and security

By participating you agree to the [Code of Conduct](./CODE_OF_CONDUCT.md).
Report security issues privately, as described in [`SECURITY.md`](./SECURITY.md).

## License

Contributions are licensed under the [Apache License 2.0](./LICENSE), as stated in its section 5.
