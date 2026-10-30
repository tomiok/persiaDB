---
name: crash-triage
description: Triage a failing crash, recovery or durability test in Persia DB (database_process kill -9 loops, database_crash / database_merge_crash SimFile tests, WAL replay, open/recovery) — including ones that look flaky or pass on rerun. Keeps the artifacts, finds the file state that loses or resurrects data, turns it into a deterministic SimFile regression test, then fixes. Use whenever such a test fails, even once, or the user says "flaky", "crash loop failed", "lost write".
---

# crash-triage

A durability test that fails once has found a bug until proven otherwise. **Never** dismiss it as flaky, rerun
until green, add `#[ignore]`, or loosen its assertion (CLAUDE.md "Things Claude must never do").

## 1. Keep the evidence
- Copy the failure message, the seed (`PERSIA_TEST_SEED=…`), and the schedule it prints (kill number, `ChildConfig`,
  `AfterAcks(n, delay)`, the in-flight operation) into your notes.
- The kill-9 harness keeps the temp dir on failure (path printed): `after-kill.persia` is the file as the kill left
  it, before any check opened it; `known.txt` the model's state. Copy them elsewhere at once and only ever work on
  copies (`cp after-kill.persia /tmp/x.persia`): an open may commit and change the file.
- Do not rebuild the test binary while a `database_process` run is in progress: children re-exec it.

## 2. Reproduce, cheaply first
- `PERSIA_TEST_SEED=<seed> cargo test -p persia-engine --test <file> -- <test>` reproduces the schedule, not the
  instant: kill points depend on timing. A non-reproducing seed is expected; it does not clear the failure.
- Compare against the base commit in a worktree (`git worktree add /tmp/base <sha>` with its own
  `CARGO_TARGET_DIR`), alone, under CPU load, and inside the full suite, to tell "my change" from "latent bug". Remove
  the worktree afterwards.
- The artifact is usually faster than reproduction: go to step 3.

## 3. Read the file state
- `cargo run -p persia-cli -- verify <copy>` gives `seq`, segments, replayed WAL records, uncommitted frames, tail.
- List the frames, `wal_start` and `log_end`, and find the lost record. `grep -boa "<doc body>" <copy>` finds its
  bytes. For frame-level detail use `persia inspect container` once it exists (ROADMAP 6.6.10); until then write a
  throwaway test (`tests/zz_debug.rs`, deleted before committing) that opens
  the copy with `Container::open(path, Mode::ReadOnly)`, prints the superblock and `frames()`, and decodes each
  `WalChunk` with `persia_engine::wal::decode_chunk`.
- Compare what the open replays (`verify`'s count) with the chunks from `wal_start`. Note anything after `log_end`
  that is not WAL (segments, sidecars, a Manifest of a cut commit), and which child wrote what (frame order).
- Write the sequence of events as numbered steps (child k cut here, child k+1 did this, the open did that). It must
  explain the exact symptom in the failure message before you change any code.

## 4. Regression test first
- Turn the sequence into a deterministic test on a `SimFile` in `tests/database_crash.rs` (or the crash test of the
  path involved): `SimFault::FailAt` / `ApplyThenFail` at every mutating call of the operation that was cut,
  `CrashLoss::{All, Nothing, Seeded(point)}`, then the follow-up operations of the scenario, another crash, and the
  reopen. Assert every acknowledged operation, through a writer open **and** a `ReadOnlyDatabase` open with
  `verify()`.
- Add a guard that the interesting state is actually reached (e.g. a count of fault points that left it, asserted
  > 0), so the test cannot silently stop covering it.
- Show it fails on the current code before fixing.

## 5. Fix, then prove it
- Fix in the engine, not the test. If an existing crash test's assertion must change because recovery now does more
  (an extra commit, a different `seq`), make it **exact** for the new behaviour, never "either is fine".
- Mutation-check the fix (`mutation-check` skill): disabling it must fail the new test.
- Open the kept artifacts with the fixed build (on copies): the lost records must come back, or document why not.
- Stress the original harness: several seeds and more kills (`PERSIA_TEST_FILES`, `PERSIA_TEST_KILLS`), then
  `just check`.
- Crash-safety changes get their own `code-reviewer` pass before anything else lands (CLAUDE.md). Update SPEC
  (§4.5, §8.6, ...) and the ADR of the open sequence or commit protocol, add a ROADMAP leaf ticked with the fix, and
  a CHANGELOG `Fixed` entry. A bug found while working on something else goes in its own branch and PR.
