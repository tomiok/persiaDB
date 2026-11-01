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
- A bug found on the way that is not part of the issue (a failing test elsewhere, a durability problem) is its own
  issue: note it, finish or park the current batch, and fix it on its own branch off `main` with its own PR. A failing
  crash, recovery or durability test is never assumed flaky: use the `crash-triage` skill.

## 4. Verify locally
```bash
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo deny check   # if dependencies changed
```
Fix everything before delegating.

## 5. Delegate (batched, see CLAUDE.md "Workflow")
Leaves are worked in batches (one ROADMAP subsection or a few related leaves per session).
1. **Per leaf, yourself:** mutation sanity check with the `mutation-check` skill: break the code (off-by-one, skipped
   check) and confirm a test fails; restore from your own backup copy, never with `git checkout`/`restore`/`stash`.
2. **`test-writer`** — only for leaves that parse untrusted bytes, change the on-disk format, or touch durability:
   "Add tests for <issue>: invariants <list>; layers: <unit/property/golden/fuzz/...>." Apply its findings; if it
   reports a bug, fix the code and keep its failing test.
3. **`code-reviewer`** — once per batch: "Review `git diff <batch-start>..HEAD` for leaves <ids> against SPEC §<x>."
   Address all BLOCKER and Major findings in one fix-up commit; justify any you decline. A leaf that changes the
   on-disk format or crash safety is reviewed on its own before the next leaf starts.
4. **`integration-tester`** — only if the change touches storage, durability, blobs, server, SDKs, sharding/distribution,
   or relaxation quality. Pass the scenario list that applies.

Re-run step 4 of "Verify locally" after changes. Leaves stay `[~]` until the batch review passes.
When every PR of the batch is open (one per leaf, or one for leaves that ship together), suggest starting a fresh
session for the next batch.

## 6. Finish
- Mark the leaf `[x]` in `ROADMAP.md`, run `cargo xtask progress --record`, and include `ROADMAP.md`,
  `PROGRESS.md` and `docs/progress-history.csv` in the PR. Add any follow-up work as new ROADMAP leaves. Both generated
  files conflict between branches that each ran it: never hand-merge them; take `main`'s versions
  (`git checkout origin/main -- PROGRESS.md docs/progress-history.csv`) and run `cargo xtask progress --record` again.
- Report the progress delta to the user (milestone %, what is now ready next).
- Update `SPEC.md`, ADRs, `CHANGELOG.md`, docs/examples if behavior is user-visible.
- Run `just check` (it also formats and lints `fuzz/`, which `cargo fmt --all` misses) before pushing.
- Output a PR description: **Context** (issue + SPEC §), **What changed**, **How it was tested** (layers + commands + seeds,
  mutation-check results), **Risks / follow-ups** (including any failure seen and not explained), **Reviewer notes**
  (what `code-reviewer` found, what was fixed, what was declined and why).
- **Open the PR** (part of the Definition of Done): push the branch and run `gh pr create --base main` with that
  description. While any other DoD item is unmet (checks failing, review pending), open it with `--draft` and list what
  remains in it; run `gh pr ready` once they are met. End the session by reporting its link. Do not open changed files
  in any editor or app.
- Last, tick the Definition of Done list from `CLAUDE.md`, the open PR included. Do not mark the issue done if any DoD
  item is unmet; list what remains.
