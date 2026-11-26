# ADR-0068: A compaction-only open: the writer's recovery with stand-in analyzers, no flush of what they would index

- Status: proposed
- Date: 2026-11-26
- Related: SPEC §4.5, §4.6, §6.2.2, §8.2.1, §8.6; ROADMAP 10.4.1, 10.4.4, 6.6.5, 6.6.8, 6.6.9; ADR-0029, ADR-0033,
  ADR-0059

## Context

`persia compact` (ROADMAP 10.4.1) rewrites a container without its dead frames (SPEC §4.6). It opened the database
with `Database::open`, the writer's open, which needs every analyzer the schemas name (SPEC §6.2.2, ADR-0059 item 5).
The command has the built-in analyzers only, so a database whose schema names an analyzer registered by its
application failed with `UnregisteredAnalyzer` (exit status 1) and could be compacted only by that application.

Read-only commands solved the same problem with `ReadOnlyDatabase::open_skipping_analysis` (ROADMAP 6.6.8): a field
whose analyzer is missing is indexed with an analyzer that emits no token. A read-only open writes nothing, so such a
memtable is harmless there. A writer is different: anything it commits is permanent. A segment flushed from a
memtable that indexed a field with no tokens would hold that field's documents without their terms, for good, and
nothing would say so.

A compaction itself needs no analysis. It copies frames verbatim (SPEC §4.6): the live frames below `wal_start`, and
every frame from `wal_start` on, so the WAL records after the last flush stay WAL records, replayed by the next open.
What needs analysis is a *flush*: turning a memtable into a segment. A writer's open flushes in two cases
(SPEC §8.6): its replay flushes a backlog larger than a memtable (ROADMAP 6.6.5), and a memtable out of docids. It
also commits once to retire a cut commit (ROADMAP 6.6.9), but that commit flushes nothing.

Two shapes were possible:
- **A compaction that takes the writer lock without replaying the WAL.** It never builds a memtable. But the writer's
  WAL positions (which chunks hold `wal_start` back, SPEC §8.2.1) come from the replay: without it, a commit of this
  handle (the one that retires a cut commit) would take a `wal_start` past records no segment holds, and lose them.
  It would also skip every check replay makes (each record decoded and validated against its schema) before the file
  is rewritten, and would need its own rules for a cut commit.
- **The writer's open, with stand-ins, that refuses to flush what the stand-ins indexed.** Everything a writer's
  open does and checks still happens, the WAL positions included; only the flushes that would need the missing
  analyzers are refused.

## Decision

1. **`persia_engine::db::CompactOnlyDatabase`**: `open(path, options)` and `open_container(container, options)` run
   the writer's open sequence (SPEC §8.6) exactly: the writer lock, the container's recovery, the manifest and every
   segment checked, the whole WAL replayed (every record decoded, validated against its schema and applied), the cut
   commit retired. A `text` field whose analyzer `options.registry` lacks (a registered one) gets the no-token
   stand-in of `open_skipping_analysis` and is listed by `skipped_analysis()`. A built-in analyzer's version this
   build lacks still fails the open as unsupported, as it does for `verify`.
2. **No flush of a skipped collection.** While a field's analysis is skipped, a commit that would flush a non-empty
   memtable of its collection is refused with `DatabaseError::AnalysisSkipped` before anything is written (the flush
   is planned, its segments built in memory, before any write). Every flush commit flushes every memtable
   with operations (ADR-0029), so such a commit is refused whole: no partial flush. In practice the refusal comes from
   the open's replay, when the backlog of such a collection reaches the memtable size threshold (`options.policy`,
   64 MiB by default) or a memtable's docids run out. The open then fails. Memtables of collections without a
   skipped field flush as in any writer's open, so what such flushes committed earlier in the replay stays: a state
   from which the next open reaches the same documents, as after any writer's open that fails part-way (SPEC §8.6).
   Without one, the file is as the container's own recovery left it (its tail repair, as any writer's open).
3. **Only a compaction.** The type offers `compact`, `space_usage`, `seq` and `skipped_analysis`, and nothing that
   writes a document, commits, merges or changes a schema: no document can be written through it, by type. Its
   writes are therefore those of a writer's open (tail repair, replay-time flushes of collections without a skipped
   field, the retiring commit, which flushes nothing) and of `Database::compact` (SPEC §4.6), which copies frames
   without reading payloads. No analysis-dependent byte is ever written.
4. **Crash safety.** The compaction is `Database::compact` unchanged: the old file or the new one at every instant,
   the WAL range and the frames after `log_end` copied, `wal_start` never moved forward (the new slot names the
   relocated old `wal_start`). The replayed records of a skipped collection are never flushed, so they stay in the
   WAL and the next writer, with the real analyzer, replays and indexes them: nothing is lost, and a deletion in the
   WAL stays a deletion (nothing resurrects). A crash at any write leaves what a crash of `Database::compact` leaves.
5. **Without a skipped field it is the writer's open.** When `options.registry` has every analyzer, nothing is
   skipped and nothing is refused: the open and the compaction are those of `Database::open` and
   `Database::compact`. So `persia compact` always opens with `CompactOnlyDatabase` (with the built-in analyzers),
   keeping its exit statuses and output, and fails only where this decision refuses (status 1).

## Consequences

- `persia compact` compacts a database whose schemas name registered analyzers, unless the open would have to flush
  such a collection's WAL backlog: then it fails (status 1) and says to open the database with its application,
  which flushes it, first.
- `DatabaseError` gains `AnalysisSkipped { collection, field, analyzer }` (code `Unsupported` in the embedded API: an
  analyzer this open lacks).
- The engine's open contract gains an entry point; `Database::open` and `ReadOnlyDatabase` are unchanged. No on-disk
  format changes.
- Rejected: replaying without flushing at the threshold, as a read-only open does. It would bound nothing (memory grows
  with the backlog until the docid limit) and only defers the refusal to that limit.
