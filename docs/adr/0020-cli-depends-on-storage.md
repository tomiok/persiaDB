# ADR-0020: The `persia` CLI may depend on `persia-storage` directly

- Status: proposed
- Date: 2026-10-17
- Related: CLAUDE.md "Architecture"; `xtask/src/check_deps.rs` (`ALLOWED`); ADR-0019; ROADMAP 2.3.8, 2.4.19, 5.4.2, 5.4.3
- Amended: 2026-10-26, `persia-engine` and `persia-format` for the segment commands (below)

## Context

`cargo xtask check-deps` lets each crate depend only on the crates listed for it, and layers may not be skipped:
`persia-cli` could depend on the `persia` facade alone. The CLI is an operator tool ("inspect, dump, verify, compact,
bench"): its first commands list and remove the temporary and quarantine files of a container or local store
(ADR-0019), which are `persia-storage` maintenance calls with no place in the embedded `Db` API. Routing them through
the facade would mean `persia-engine` and `persia` re-exporting storage internals as public API.

## Decision

`persia-cli` may depend on `persia` and on `persia-storage`. Later commands that dump or verify the on-disk format
may add `persia-format` by amending this ADR. The library crates' layering is unchanged: `persia` still reaches
storage only through the engine and the blob store, and what users import from `persia` does not grow.

## Amendment (2026-10-26): segment commands

`persia inspect segment` and `persia dump segment` (ROADMAP 5.4.2, 5.4.3) read one segment file with the engine's
segment reader (`persia_engine::segment::Segment`, which takes a `persia_format::ByteSource`). A segment on its own is
not something the embedded `Db` API exposes, nor should it. So `persia-cli` may also depend on `persia-engine` and
`persia-format`, as this ADR anticipated for format tools. The rule is the same as for storage: these are operator
commands on the on-disk format, and anything user-facing still goes through `persia`.

## Consequences

- Maintenance commands call the storage layer directly, with its own error types; the CLI turns them into messages.
- The CLI binary is the only crate above the engine that sees `persia-storage`, `persia-engine` and `persia-format`
  directly; a new user-facing API must still go through `persia`.

## Alternatives considered

- **A `persia::maintenance` module re-exporting storage calls**: keeps the rule literal, but makes storage internals
  part of the public embedded API and adds pass-through modules to two crates that are still stubs.
- **A separate `persia-admin` library crate**: one more crate for a handful of calls, with the same dependency.
