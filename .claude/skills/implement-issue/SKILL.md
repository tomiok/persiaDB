---
name: implement-issue
description: End-to-end workflow for implementing one Persia DB GitHub issue / ROADMAP leaf. Use whenever the user says "implement issue", "work on 1.2.3", "next roadmap item", or hands over an issue link. Plans from SPEC.md, implements, then delegates to the test-writer, code-reviewer and (when relevant) integration-tester subagents before declaring done.
---

# implement-issue

Implements **one** issue = one PR, following SPEC.md and CLAUDE.md.

## 1. Understand
1. Read `PROGRESS.md` for current state, then the issue (or find the leaf in `ROADMAP.md` by id, e.g. `5.2.2`). Note its **Acceptance** and **Tests** sections.
2. Read the SPEC sections it cites, and `CLAUDE.md` rules relevant to the crate (format discipline, unsafe policy, dependency policy).
3. Skim neighboring code and existing tests to match conventions. Check the roadmap dependencies are done; if not, say so and stop.
   (`PROGRESS.md` shows which milestones are ready.) Mark the leaf `[~]` in `ROADMAP.md`.
4. If SPEC is ambiguous or contradicts the issue: **stop and ask**, or propose a SPEC change. Never guess on format or protocol details.

## 2. Plan (write it down before coding)
Produce a short plan in the conversation:
- Files/modules to add or change; public API surface (keep minimal).
- Invariants the code must hold (these drive the tests).
- Failure modes: corruption, crash points, hostile input, overflow.
- Whether the change touches the on-disk format (if yes: SPEC update + version/flag bump + ADR + golden test are part of this PR).
- Performance sensitivity (hot path? needs a benchmark).

## 3. Implement
- Smallest change that satisfies the issue. No drive-by refactors; note them as follow-up issues instead.
- Follow coding rules in `CLAUDE.md` (no `unwrap` on external data, checked arithmetic, `SAFETY` comments, English docs).
- Commit in small logical steps with conventional commits.

## 4. Verify locally
```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo deny check   # if dependencies changed
```
Fix everything before delegating.

## 5. Delegate (in order)
1. **`test-writer`** — "Add tests for <issue>: invariants <list>; layers: <unit/property/golden/fuzz/...>." Apply its findings; if it reports a bug, fix the code and keep its failing test.
2. **`code-reviewer`** — "Review the current diff for <issue> against SPEC §<x>." Address all BLOCKER and Major findings; justify any you decline.
3. **`integration-tester`** — only if the change touches storage, durability, blobs, server, SDKs, sharding/distribution, or relaxation quality. Pass the scenario list that applies.

Re-run step 4 after changes. Re-run `code-reviewer` if you made significant edits after its review.

## 6. Finish
- Tick the Definition of Done list from `CLAUDE.md`.
- Mark the leaf `[x]` in `ROADMAP.md`, run `cargo xtask progress --record`, and include `ROADMAP.md`,
  `PROGRESS.md` and `docs/progress-history.csv` in the PR. Add any follow-up work as new ROADMAP leaves.
- Report the progress delta to the user (milestone %, what is now ready next).
- Update `SPEC.md`, ADRs, `CHANGELOG.md`, docs/examples if behavior is user-visible.
- Output a PR description: **Context** (issue + SPEC §), **What changed**, **How it was tested** (layers + commands + seeds),
  **Risks / follow-ups**, **Reviewer notes**.
- Do not mark the issue done if any DoD item is unmet; list what remains.
