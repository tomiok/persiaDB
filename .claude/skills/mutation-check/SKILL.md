---
name: mutation-check
description: Mutation sanity check of a leaf's tests (CLAUDE.md Workflow, implement-issue step 5.1): break the code on purpose in a few targeted ways, confirm a test fails for each, and restore the exact bytes. Use after implementing any ROADMAP leaf, before review, or when the user asks whether the tests "would catch" a bug.
---

# mutation-check

Proves the new tests bite: each mutant (a deliberate bug) must make at least one test fail. A mutant that survives
is a test gap: add or strengthen a test, then re-run that mutant.

## Rules (safety first)
- **Never** restore with `git checkout`, `git restore`, `git stash` or `git reset`: they also discard the leaf's
  uncommitted work in the same files. Restore only from your own backup copy.
- Back up every file you mutate **before** the first mutant, outside the repo:
  `mkdir -p /tmp/mut && cp <file> /tmp/mut/<name>`.
- After each mutant, restore with `cp /tmp/mut/<name> <file>`, and finish with `cmp /tmp/mut/<name> <file>` for
  every backed-up file; report `restored` only when every `cmp` is silent.
- Delete any `*.proptest-regressions` file a mutant created (`git status --short` shows it as new): it records a
  failure of the mutant, not of the real code.
- One mutation at a time; never commit while a mutant is in place.

## 1. Pick mutants (3–8 per leaf)
Target the invariants the leaf's tests claim (the test file's "Invariants:" header, the SPEC section). Prefer
mutants a plausible bug would produce:
- a bound or comparison off by one (`<` → `<=`, `checked_sub(1)` removed);
- a check skipped (a validation `if` short-circuited, a CRC or length check removed);
- a term dropped from a sum, `max` ↔ `sum`, `min` ↔ `max`;
- a field or flag ignored (always `None`, always `0`, the first element instead of all);
- an order changed (write order, commit before data, a sort key);
- for floating point: the textbook form of a formula instead of the monotonic or stable one.

Each mutant is one `sed -i` (or `Edit`) on one line, and the pattern must match exactly once (`grep -c` first).

## 2. Run
Run only the tests that should catch it, quietly, and keep just the verdict:

```bash
cargo test -q -p <crate> --lib --test <file> -- <filters> 2>&1 | grep -E "test result|FAILED|panicked" | head -3
```

Proptest-based tests can miss a rare mutant on a few cases; if one survives, rerun with `--features slow` before
calling it a gap. For a mutant only an exact float or rounding case exposes, a deterministic test (adjacent floats
with `next_up`, a fixed tiny weight) beats more random cases.

## 3. Report
A table: mutant (file:line, what changed) → caught by (test name) / **survived**. For each survivor: the test added
or strengthened, and the rerun showing it now fails on the mutant and passes on the restored code. End with the
`cmp` check.
