# Architecture decision records

An ADR records one significant decision: the context, what we chose, and what it costs.
SPEC.md says *what* the system does; ADRs say *why* it is that way and what was rejected.

## When to write one

- Any change to the on-disk format (required by CLAUDE.md "Format discipline", together with a SPEC update,
  a version or flag bump and a golden test).
- Resolving an open question in SPEC §18.
- Adding a dependency outside the allowed list, or new `unsafe` (CLAUDE.md "Safety").
- Any decision a future contributor would otherwise re-litigate.

## Process

1. Copy [`0000-template.md`](./0000-template.md) to `NNNN-short-title.md`, using the next free number.
2. Open it with `Status: proposed` in the same PR as the change it justifies (or alone, for a pure decision).
3. On merge it becomes `accepted`. ADRs are never edited afterwards except for status: to change a decision,
   write a new ADR and mark the old one `superseded by ADR-XXXX`.
4. Add it to the index below and link it from the SPEC section it affects.

## Index

| ADR | Title | Status |
|---|---|---|
