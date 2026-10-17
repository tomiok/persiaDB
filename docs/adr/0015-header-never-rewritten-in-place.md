# ADR-0015: The header page is never rewritten in place; `format_minor` is recorded by rewrites

- Status: accepted
- Date: 2026-10-16
- Related: SPEC §4.2, §4.6; ADR-0007, ADR-0008; ROADMAP 2.4.14

## Context

SPEC §4.2 asks a writer to record `format_minor = max(the container's minor, its own minor)`. ADR-0008 settled the
value but not when it is written. A read-write open by a newer build could rewrite the header page in place, but
the header has no second copy: under ADR-0008's own failure model (a device writes a 4 KiB unit atomically or not
at all, and power loss during the write may leave the sector unreadable), a torn header write would make the whole
container unreadable. The superblock slots survive torn writes only because there are two of them.

`format_minor` is diagnostic: readers never use it to decide what to read (ADR-0008); feature flags do that.

## Decision

1. The header page is written only into a file that is not yet published: by create (a temporary file linked into
   place) and by a rewrite (compaction, §4.6, renamed into place). No operation writes the header of a published
   container.
2. So `format_minor` records the newest minor that created or rewrote the container. A read-write open by a newer
   build does not change it; the next compaction records `max(stored, own)`.
3. Any future change that must reach the header of an existing container (a new `incompat_flags` or
   `rocompat_flags` bit) does it by a rewrite as well, or brings its own crash-safe scheme in its ADR.

## Consequences

- A torn write can never damage the header; only the double-buffered slots are written in place.
- Between a newer build's first write and the next compaction, `format_minor` understates which builds have written
  the container. Diagnostics must treat it as a lower bound.
- Raising a feature flag on an existing container costs a rewrite. Rare by design (SPEC §15).

## Alternatives considered

- **Rewrite only the 6 bytes of `format_minor` and `header_crc` in place**: they share one 512-byte sector, but a
  4Kn device rewrites the whole 4 KiB page, and an unreadable header page loses the container.
- **Keep the writer's minor in the superblock** (double-buffered, so torn writes are survivable): a format change for
  a diagnostic field, and two places that disagree about the minor.
- **Rewrite the whole container on the first read-write open by a newer build**: correct, but an open could then
  cost a full copy of a large database.
