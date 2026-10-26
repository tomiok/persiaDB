# ADR-0026: A counting global allocator in allocation tests

- Status: proposed
- Date: 2026-10-26
- Related: ROADMAP 4.4.4, 4.4.12; CLAUDE.md "Coding rules" (the unsafe policy); ADR-0010 (the mmap unsafe boundary)

## Context

ROADMAP 4.4.4 and 4.4.12 promise that analysis allocates nothing once a `TokenStream` has grown. A test can only check
that promise by counting allocations, and Rust offers that through a `#[global_allocator]`, whose `GlobalAlloc`
trait is `unsafe` to implement. CLAUDE.md allows `unsafe` only where an ADR lists it (`UNSAFE_ALLOWED` in
`xtask/src/repo_checks.rs`). The alternative, a dev-only crate such as `dhat`, adds a dependency tree (backtraces,
serialization) for about twenty lines of code, against the "write the 100-line primitive" rule.

## Decision

`crates/persia-analysis/tests/allocations.rs`, a test binary of its own, installs a global allocator that forwards
every call to `System` unchanged and counts `alloc`, `alloc_zeroed` and `realloc` in a `const`-initialized
thread-local `Cell` (it never allocates, and `try_with` tolerates thread teardown). Counting per thread keeps the
parallel tests of that binary from disturbing each other. The file `allow`s `unsafe_code`, every `unsafe` block has a
`SAFETY` comment, and the file is listed in `UNSAFE_ALLOWED`. Other crates that need allocation tests copy the
pattern into their own test file and list it here.

## Consequences

- The allocation-free claim is checked in CI for every built-in analyzer: once the counts of a round of values are
  zero four times in a row, the stream has stopped growing.
- The `unsafe` lives in a test binary only; no library code changes, and Miri is unaffected (it does not run
  `persia-analysis` tests).

## Alternatives considered

- **`dhat` (dev-dependency):** no `unsafe` in the repo, heap profiles for free, but a dependency tree not on the
  allowed list for a small primitive.
- **Inspecting buffer addresses and capacities** (the 4.4.4 unit test): needs no `unsafe`, but sees only the stream's
  own buffers, not allocations inside filters, which is exactly what 4.4.12 is about.
