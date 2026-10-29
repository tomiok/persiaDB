# ADR-0033: Open sequence, read-only opens, and flushes during replay

- Status: proposed
- Date: 2026-10-29
- Related: SPEC §4.5.1, §8.2, §8.2.1, §8.4, §8.6; ROADMAP 6.6.1–6.6.5; ADR-0028, ADR-0029

## Context

ROADMAP 6.3 already opened a database: container recovery, manifest checks, segments, then `replay_container` into
fresh memtables. Three things were missing. Replay could not commit, so a backlog larger than a memtable (one
collection's unflushed record holds `wal_start` back for every collection, ADR-0028 Consequences) failed the open
with `SegmentError::Full` at the docid limit, and ignored the size threshold. There was no way for another process to
read a database a writer holds (SPEC §8.4 promises readers). And nothing checked a whole database: an open checks
only what it needs, and `Segment::verify` ran only on merged segments.

## Decision

1. **One open sequence for both modes** (SPEC §8.6): container recovery, manifest, segments and sidecars, replay,
   ready. `db::recovery` holds it; the writer and the read-only open differ only in what replay may do.
2. **The writer replays inside `Database`.** It is assembled before replay (with a WAL writer that holds no chunks),
   then applies each record through the same function `Replay` uses, so it can commit in between:
   - before a record, if its memtable reached the size threshold (the write path's `flush_if_full`);
   - when the memtable rejects a record for lack of room (`SegmentError::Full` or `TooLarge`, and the memtable is not
     empty), then the record is applied again to the fresh memtable. Any other rejection fails the open as before.
   The commit is the normal one (every memtable flushed, ADR-0029). Its `wal_start` is capped at the start of the
   chunk being replayed (`Database::replaying`), since the rest of that chunk and the later chunks are not in a
   memtable yet. Records of that chunk applied before the flush are replayed again after a crash, which is
   idempotent (ADR-0028 item 5).
3. **Replayed records hold `wal_start` through the WAL writer** (`WalWriter::replayed`, one entry per collection,
   the first chunk since its last flush), exactly as `WalWriter::resume` did with `ReplaySummary::pending`. A commit
   that flushes nothing (a collection create, a merge commit) keeps them needed.
4. **`ReadOnlyDatabase`** runs the sequence on a read-only container: no lock, no write, no truncation (SPEC
   §4.5.1). It keeps the snapshot it found; following a writer's later commits is a reopen. Replay stops at the
   first Manifest frame after the loaded `log_end` whose `seq` is newer than the loaded manifest, so a commit that
   lands during the scan (a new collection, then records logged after that commit) is not mixed into the snapshot.
   It cannot flush, and it does not stop at the size threshold, so memory grows with each collection's unflushed
   backlog until the memtable's docid limit (about 2^31 operations) fails the open. A read-write open flushes at
   the size threshold and so bounds that memory.
5. **`ReadOnlyDatabase::verify`** (and `persia verify`) adds to the open: `Segment::verify` on every live segment, at
   most one live copy of each id per collection (SPEC §8.3; it keeps the live ids of one collection in memory), and
   corruption after `log_end` as an error (`DatabaseError::CorruptTail`), except what a read-write open truncates
   silently: a torn tail, or zeros and stale frames (`Recovery::stale_tail`, which read-only container opens now
   classify too), reported as `Tail::Torn` and `Tail::Stale`. A container whose read-write open already quarantined
   other corrupt bytes and truncated them reports `Tail::Clean`: those bytes are no longer in the file
   (`Recovery::truncated_from`). A read-only classify that loses the tail to a writer's truncation (a short read)
   leaves `stale_tail` clear and does not fail the open. New
   corruption variants `DatabaseError::LiveTwice` and `CorruptTail`.
6. **The writer lock is the container's** (SPEC §4.5.1): a second `Database::open` fails with
   `DatabaseError::Container(Locked)`; no database-level lock is added.

## Consequences

- An open may commit, so it may fail with any commit error, and a crash inside it leaves a later manifest; the next
  open reaches the same documents (tested at every write of such an open).
- A read-only reader sees the WAL records written after the last commit, as a writer's open would (an `Async` one
  before it is synced, so a power loss may still drop it), but never a record the writer only buffered.
- `verify` costs a full read of every segment and memory for one collection's ids.

## Alternatives considered

- **Flushing only the full collection during replay**: a commit then leaves other memtables' records before its
  `wal_start`, against ADR-0029's rule that every flush flushes all; the cap of item 2 would still be needed.
- **A `wal_start` exactly at the record**: `wal_start` is a frame start (SPEC §4.5), and a record offset inside a chunk
  would be a manifest format change.
- **Read-only replay into several memtables per collection** to avoid the capacity failure: a snapshot holds one
  memtable per collection; not worth it for a case that needs 2^31 operations.
- **Refreshing a `ReadOnlyDatabase` in place**: possible later (a new snapshot from the next manifest and the new WAL
  frames); a reopen does the same now.
