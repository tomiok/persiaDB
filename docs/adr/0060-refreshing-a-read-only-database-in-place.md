# ADR-0060: Refreshing a read-only database in place

- Status: proposed
- Date: 2026-11-22
- Related: SPEC §4.5.1, §4.6, §8.2.1, §8.4, §8.6, §8.7; ROADMAP 6.6.2, 6.6.6, 6.6.8, 6.6.9; ADR-0028, ADR-0029,
  ADR-0033 (its Alternatives named this), ADR-0054, ADR-0059

## Context

A `ReadOnlyDatabase` shows the snapshot its open found and never follows a writer (ADR-0033 item 4). Following one
meant a new open: every segment read again and every WAL record from `wal_start` replayed again, even when the writer
had only logged a few puts since. A reader that refreshes often (a server's replica, SPEC §11.4) pays a full open each
time, and has to swap the new database in under its own lock.

## Decision

1. **`ReadOnlyDatabase::refresh(&self) -> Result<Refreshed, DatabaseError>`.** The database keeps its current state
   (container, snapshot, replay counters, the WAL chunk frames replayed, the skipped fields) behind an `ArcSwap`; a
   refresh builds the next state and swaps it in with one store. `snapshot()`, `seq()`, `replayed()`, `wal_end()`,
   `container()`, `manifest()`, `skipped_analysis()` and `objects()` read the current one; a search, or anything
   else holding a `Snapshot`, keeps the one it took. Refreshes are serialized by a mutex that also holds the
   memtables replay writes into; reads never take it.
2. **Reopen the container.** The refresh reopens the container read-only and recovers it anew
   (`Container::reopen_read_only`): at its path when it was opened by path, so it follows a compaction that renamed a
   new file over it (SPEC §4.6), else on another handle of its file (`ContainerFile::try_clone_handle`:
   `File::try_clone`, a `SimFile` clone; other implementations report `Unsupported` unless they provide it). Another
   `db_uuid` there, or an older committed `seq`, is refused (`DatabaseError::RefreshRefused`, not corruption):
   nothing changes. A reader that wants to follow such a file opens again.
3. **Incremental when sound**, `Refreshed::Incremental`: the WAL chunk frames already replayed are the first ones the
   new scan finds from the new `wal_start`, frame for frame (offset, length, `frame_seq`), the last of them still has
   the payload it had (its CRC-32C, kept from the replay), and the commits since, if any, dropped no collection and
   changed no schema. The prefix test covers `wal_start`: a commit that moved it past a replayed chunk flushed records
   the memtables hold, and that chunk is not found again; one that moved it only over chunks this reader never
   replayed (it replayed none) leaves the memtables as a fresh open builds them. So a merge, a commit that only
   retires a cut one (ROADMAP 6.6.9), a created collection, and a flush of records written after this reader's scan
   are incremental. Then only the chunks after the replayed ones are replayed (with the read-only stop of SPEC §8.6)
   into the same memtables, a created collection gets an empty memtable analyzed as the open's would be, and segments
   are opened only where the manifest's entry changed; unchanged ones are shared with the old snapshot. While it
   replays, the refresh holds every memtable (`Memtable::hold`, as a batch does, ROADMAP 6.3.12), and releases them
   after the swap, so no reader's view sees part of a refresh.
4. **Full reload otherwise**, `Refreshed::Reloaded`: a replayed chunk is no longer found (a flush commit, another file
   at the path, a power loss that dropped an unsynced tail), the last one's payload changed, a collection was dropped
   or its schema changed, the same `seq` names another manifest or `wal_start`, or an earlier refresh failed after it
   changed the memtables. The open sequence runs on the reopened container into new memtables (SPEC §8.6), with the
   open's registry and analysis mode (ROADMAP 6.6.8: `UnregisteredAnalyzer`, or the skipping open of `persia verify`).
5. **`Refreshed::Unchanged`** when the commit and the replayed chunks are the same and no chunk follows them: nothing
   is swapped.
6. **A failed refresh changes nothing readers see.** An error before replay (reopen, refusal, manifest, segment,
   analyzer) leaves the state as it was. An error during an incremental replay (a record that does not apply) leaves
   the memtables it wrote held, so views keep showing the published state, and marks them unusable: the next refresh
   reloads in full. A refresh that panicked is treated the same (the mutex's poison is cleared and the next refresh
   reloads).

## Consequences

- `container()` and `manifest()` now return `Arc`s of the current state (they borrowed the open's), and
  `skipped_analysis()` a `Vec`; `objects()` holds its container (`ContainerObjects::shared`, so it needs
  `F: 'static`). `verify` and `blob_references` read one state throughout, so a refresh during them cannot pair a
  snapshot with another container.
- A refresh costs a container recovery (header, superblock, the frames from `wal_start`, the scan from `log_end`),
  one re-read of the last replayed chunk, the new WAL frames and the changed segments; a full reload costs an open,
  as a reopen did.
- A snapshot taken before a refresh that keeps the memtables shares them with the new one, so a view taken from it
  after the refresh sees the new records, as a writer's snapshot sees later writes (SPEC §8.4). A view taken before,
  and so a search in progress, sees no change.
- An incremental refresh applies records to memtables in place, as replay does: the reader's memory grows with the
  backlog it follows until a flush commit makes the next refresh a full reload (ADR-0033 item 4).
- The divergence checks are best-effort, as `ReadOnlyBlobDatabase::refresh`'s are (SPEC §9.3): another history whose
  replayed chunks match frame for frame and whose last replayed chunk is byte-identical is taken.
- `ReadOnlyBlobDatabase` keeps refreshing with a new open (`refresh(reopened)`), which pairs the documents with the
  pack refresh and the reader lease (ADR-0054). Calling `refresh` on its `database()` advances the documents without
  the pack: a blob bound since then reads as not found until the blob database is refreshed.

## Alternatives considered

- **`refresh(&mut self)`**: simpler (no `ArcSwap`, no mutex), but every reader would share the database under an
  outside lock, and a refresh would wait for them.
- **Mutating the open's `Container` in place** (a read-only rescan): every frame read is bounded by the container's
  end, so the container must change, and `container()` could no longer return a plain reference while a refresh may
  run; a new container per refresh is the same open code.
- **Always a full reload**: correct, and what `ReadOnlyBlobDatabase::refresh` does with a new open today; it re-reads
  every segment and the whole WAL backlog on each refresh.
- **Comparing every replayed chunk's payload**: catches more divergence, but re-reads the whole backlog on each
  refresh, which is what the leaf removes. A power loss drops an unsynced suffix, so the last chunk is where another
  history first shows.
- **Incremental across a flush of replayed records** (new segments, memtables rebuilt from the new `wal_start` while
  unchanged segments are kept): saves segment reads, but rebuilds the memtables anyway; left for when a measured
  workload needs it.
