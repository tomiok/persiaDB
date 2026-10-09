## Context
<!-- ROADMAP leaf (x.y.z), issue link, SPEC §. -->

## What changed

## How it was tested
<!-- Layers (unit / property / golden / fuzz / differential / crash / integration), commands, seeds. -->

## Risks / follow-ups
<!-- Follow-up work goes into ROADMAP.md as new leaves. -->

## Definition of Done (CLAUDE.md)
- [ ] Behavior matches SPEC (cite section), or SPEC updated in this PR
- [ ] `just check` passes (fmt, clippy `-D warnings`, tests, repo checks, cargo deny)
- [ ] New tests at the right layers; fuzz target if bytes are parsed
- [ ] No new `unsafe` without `SAFETY` + ADR; no `unwrap` on external data
- [ ] Public API documented; CHANGELOG entry if user-visible
- [ ] Benchmarks added/updated if on a hot path
- [ ] On-disk format change: SPEC + version/flag bump + ADR + golden test
- [ ] ROADMAP leaf ticked and `PROGRESS.md` regenerated
