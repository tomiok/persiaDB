# ADR-0013: `frame_seq` numbers a container's frames contiguously; the superblock anchors it

- Status: accepted
- Date: 2026-10-15
- Related: SPEC §4.3, §4.4, §4.6, §8.6, §15; ADR-0008, ADR-0009; ROADMAP 2.4.4, 2.4.9, 2.4.10

## Context

Every frame header has a `frame_seq u64` covered by the CRC, but SPEC §4.4 left its meaning open, and ADR-0009
deferred two questions to ROADMAP 2.4.9: what the field numbers (and its first value after a rewrite, §4.6), and
whether a scan stops at a `frame_seq` that does not increase. Recovery needs the answer now (ROADMAP 2.4.4), and
ROADMAP 2.4.10 needs a way to tell frames that belong after the scan's stop offset from leftovers.

The scan rules of ADR-0009 judge each frame alone. A frame that is valid but does not belong where it sits (a
leftover of an earlier write, a block from another file, a misdirected write that landed whole) passes them.

No format version has been released, so the v1.0 layout is amended in place (SPEC §15, ADR-0008).

## Decision

1. **What it numbers.** `frame_seq` counts the frames of one container file in append order. Each frame's
   `frame_seq` is one more than that of the frame before it, Padding and unknown kinds included, so the frames of a
   file form one contiguous run. The first frame of a new container has `frame_seq = 1`.
2. **Rewrites continue the count.** A rewrite (§4.6) numbers the frames of the new file on from one more than the
   `frame_seq` of the last valid frame of the old file. That is not the old superblock's `next_frame_seq` when
   acknowledged WAL frames follow `log_end` (§8.2): they already hold those numbers. The new file's slots are both
   commits (`seq ≥ 1`), never the `seq = 0` state of a new container, whose `next_frame_seq` is 1. So within one database a `frame_seq` is never given to two different frames, even across rewrites.
   A frame copied by a rewrite gets a new number. `frame_seq` therefore stays monotonic over the database's life,
   which later WAL or replication bookkeeping may rely on; nothing does yet.
3. **The superblock anchors the run.** The superblock gains `next_frame_seq u64` at offset 48 (taken from the
   reserved bytes): the `frame_seq` of the frame at `log_end`, i.e. one more than the last committed frame's. A new
   container stores 1. Without the anchor the first frame after `log_end` could not be checked, and a container
   whose last commit has no Manifest frame at `log_end` (a new one) would need a special case.
4. **Recovery checks it** (§4.4, §8.6):
   - the scan from `log_end` expects `next_frame_seq` at `log_end` and one more for each later frame. A valid frame
     with any other value ends the scan as *corruption* at that frame: it is whole, so it is not a torn tail and is
     never truncated as one (ADR-0009). What a read-write open then does with it is ADR-0014's: a frame below the
     expected value is a leftover and may be truncated; one at or above it may be acknowledged and is kept;
   - frames in `wal_start..log_end` were committed: they must be contiguous and the last one must have
     `next_frame_seq - 1`, else the container is corrupt. Frames below `wal_start` are checked when read, by kind.
5. **Range.** Values are in `1..u64::MAX`. A writer never appends a frame with `frame_seq = u64::MAX`, so
   `next_frame_seq` always fits (at one frame per microsecond, exhausting it takes over 500,000 years), and 0 is
   never written, so a checked scan rejects both. A committed superblock has `next_frame_seq ≥ 2`: a commit has at
   least its Manifest frame.

## Consequences

- A whole but misplaced frame after `log_end` (a leftover, a misdirected write) is reported instead of being
  replayed as acknowledged data.
- ROADMAP 2.4.10 can tell which valid frames past a corruption continue the run (`frame_seq` greater than that of
  the last valid frame before it) and which cannot belong.
- Recovery must read the frames of `wal_start..log_end`, which it does anyway to replay the WAL (§8.6).
- The golden fixtures change (the superblock field). They were already written with contiguous `frame_seq` from 1.
- A database copied by a rewrite does not start at `frame_seq = 1`. Tools must not assume it does.

## Alternatives considered

- **Derive the anchor from the Manifest frame at `log_end`**: commits put the Manifest last (§4.5), but a new
  container has none, and the rule would tie frame order inside a commit to the recovery check. For the same
  reason the Manifest is not used as a cross-check either (e.g. `manifest_off + manifest_len = log_end`): a writer
  may append frames after the Manifest and before the slot write, and they are committed with it.
- **Monotonic but not contiguous**: a scan could accept gaps, so a lost frame between two valid ones would go
  unnoticed. Contiguity costs nothing for a single appending writer.
- **Restart at 1 after a rewrite**: simpler, but then the same `frame_seq` names different frames before and after
  a rewrite, which any future cross-file bookkeeping (replication of the WAL, quarantine copies) would trip over.
- **Treat a mismatched `frame_seq` as a torn tail**: it would truncate a whole frame that may hold data. Corruption
  keeps the bytes.
