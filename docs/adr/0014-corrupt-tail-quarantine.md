# ADR-0014: A read-write open quarantines a corrupt tail unless it may hold acknowledged writes

- Status: accepted
- Date: 2026-10-16
- Related: SPEC §4.4, §4.5.1, §8.2; ADR-0009, ADR-0013; ROADMAP 2.4.10

## Context

The scan from `log_end` ends in *corruption* (ADR-0009) when the frame at the stop offset `o` is invalid and
non-zero bytes follow, or when a whole frame breaks the `frame_seq` run (ADR-0013). Until now a read-write open
then failed (`CorruptTail`), so a database whose writer crashed during an append whose pages reached the disk out
of order (delayed allocation, a lost first sector) could not be written again without manual work, although no
acknowledged data was at stake. The bytes alone cannot tell that crash from a damaged synced frame, but
`frame_seq` narrows it: a valid frame after `o` whose `frame_seq` is lower than the one expected at `o` can never
be acknowledged data of this file's run.

## Decision

On a scan that ends in corruption at `o`, a read-write open searches `o..end` for valid frames, trying every
8-aligned offset that holds the frame magic (frames are 8-aligned) and skipping valid frames whole. Then:

1. A valid frame with `frame_seq` ≥ the one expected at `o`, or a frame valid but for flags this build does not know
   (a newer writer's, which nothing may truncate, SPEC §4.4), may be an acknowledged write (a synced WAL frame behind a
   damaged one): the open fails with `CorruptTail`, naming that frame's offset, and changes nothing. A caller may ask
   for `Repair::Quarantine`, which treats it like case 3.
2. Only zeros and stale frames (`frame_seq` below the expected one): truncated at `o` like a torn tail.
3. Anything else: the bytes `o..end` are first copied to `<name>.quarantine.<o as 16 hex digits>.<nonce>.<n>` next to the
   container, the copy is synced and its directory synced; only then is the container truncated at `o` and synced.
   The open reports the copy's path (`Recovery::quarantined`).

The search and the copy read in 64 KiB chunks. Every 8-aligned word holding the magic is checked as a frame, which
reads the extent its header claims, so a hostile tail could make the search quadratic: it may read at most four times
the tail's length, and past that budget nothing is known and the open fails as in case 1 (without an offset). A container opened without a path (`open_on`) has nowhere to put a
copy: case 3 fails like case 1, without an offset. A read-only open never changes anything (unchanged).

## Consequences

- The common crash patterns no longer block writers; nothing that might be acknowledged is lost silently, and the
  bytes that are removed are kept in a file the operator can inspect or delete.
- A crash at any step leaves the container unchanged, or truncated with a durable quarantine copy (tested on the
  simulated file system, ROADMAP 2.4.11).
- Quarantine files accumulate if corruption recurs; they are never removed by Persia (a `persia` CLI command may list
  them, together with the stale temporary files of ROADMAP 2.4.15).
- After a truncation at `o`, new frames are numbered on from the `frame_seq` expected at `o`, so they reuse values that
  frames in the quarantine file may carry: the quarantine file is evidence, not a WAL to re-apply as is.
- A frame inside the garbage that happens to look valid with a high `frame_seq` (e.g. a container stored as a
  payload) makes the open refuse: a false alarm, never a loss.

## Alternatives considered

- **Always refuse** (the previous behavior): safe but turns an ordinary crash into an outage.
- **Truncate without a copy when no candidate frame exists**: the bytes might be a damaged synced frame whose loss
  an operator would want to investigate; the copy costs little.
- **Resume the scan after the damaged frame** (skip it and keep later frames): would replay a WAL with a hole in it;
  the engine cannot know what the lost frame held.
