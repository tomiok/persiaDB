# ADR-0068: A compaction-only open: the writer's recovery with stand-in analyzers, no flush of what they would index

- Status: proposed; amended 2026-11-27 (ROADMAP 10.4.8: a pre-scan refuses a WAL holding a pending record of a
  skipped collection before the replay, writing nothing; item 2's flush guard stays as a backstop)
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
   memtable of its collection is refused with `DatabaseError::AnalysisSkipped` before anything is written: it is
   checked before the flush is planned, so before any segment is built or segment id taken. Every flush commit
   flushes every memtable with operations (ADR-0029), so such a commit is refused whole: no partial flush. In practice
   the refusal comes from the open's replay, when a replay-time flush is due, of any collection, while such a
   collection holds a replayed record: a memtable reaching the size threshold (`options.policy`, 64 MiB by default;
   for such a collection that is measured on the stand-in memtable, which holds none of the skipped field's terms)
   or running out of docids. The open then fails. Memtables of collections without a
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

- `persia compact` compacts a database whose schemas name registered analyzers, unless its open's replay would flush
  (any collection) while such a collection holds a replayed record: then it fails (status 1) and says to open the
  database with its application and commit (closing a `persia::Db` commits; an open alone may flush nothing), then
  compact.
- `DatabaseError` gains `AnalysisSkipped { collection, field, analyzer }` (code `Unsupported` in the embedded API: an
  analyzer this open lacks).
- The engine's open contract gains an entry point; `Database::open` and `ReadOnlyDatabase` are unchanged. No on-disk
  format changes.
- Rejected: replaying without flushing at the threshold, as a read-only open does. It would bound nothing (memory grows
  with the backlog until the docid limit) and only defers the refusal to that limit. Since every flush flushes every
  memtable (ADR-0029), skipping only the skipped collection's flushes is not possible either: it would stop every
  collection's flushes while that collection holds a record, and leave all their backlogs unbounded.

## Amendment (2026-11-27, ROADMAP 10.4.8): a pre-scan refuses before the replay

Item 2 made the refusal depend on where a flush threshold fell. When the replay flushed a collection without a skipped
field and only then reached a record of a skipped collection, the open was refused after it had committed that flush,
with `wal_start` still at the chunk being replayed (SPEC §8.6). Each refused `persia compact` or `persia gc` then
replayed the same records and flushed them again into new segments: nothing was lost or resurrected, but the container
grew with every refusal until the application committed (found in the ROADMAP 10.4.7 integration tests). The owner
decided:

1. **The pre-scan.** Before its replay (step 4 of SPEC §8.6), an open that skips analysis reads the chunks the
   writer's replay reads once, decoding every record as the replay does (the replay's own decoder: the chunk, the
   collection in the loaded manifest, a put's document with that collection's schema) and applying none. If any
   *pending record* (below) belongs to a collection with a skipped field, the open fails with
   `DatabaseError::AnalysisSkipped` at once, naming the first such field in collection then field order. Where that
   record sits relative to a flush threshold does not matter.
2. **A pending record** of a collection is a record the replay would apply to it: a put or a delete of that
   collection, of any id, from `wal_start` on (after `log_end` and after a cut commit's Manifest included), before
   the first record the replay cannot apply. So records a segment already holds (a chunk replayed again after a
   replay-time flush) are pending; records before `wal_start` are not; a drop or schema change is a commit after
   which no earlier record is replayed; a cut commit never took effect, so the records around it are pending; a
   record of a collection the manifest lacks is corruption, not pending. Where in doubt, the rule refuses: an
   accepted open never replays a record of a skipped collection, so no replay can need to flush its memtable.
3. **A refused open writes nothing** but the container's tail repair, as any writer's open: no segment, no segment id,
   no commit, and no retiring commit of a cut commit (item 1's retiring commit happens only in an accepted open).
   N refusals in a row leave the container byte-identical.
4. **Damaged bytes.** A record the pre-scan cannot decode gives the replay's error for it (corruption, or a newer
   encoding). After a pending record the open fails with that error at once, before anything is written: the
   application's open, with the analyzer, fails on the same bytes, so the refusal's remedy would not help. Before any
   pending record nothing is refused, and the replay runs and fails on it exactly as in a writer's open. One extra read
   of the WAL range, only when a field is skipped; no speed claim.
5. **Item 2's flush guard stays**, as a backstop (defence in depth): `Database::plan_flushes` still refuses a commit
   that would flush a non-empty memtable of a collection with a skipped field, before any segment is built or id
   taken. After the pre-scan no such memtable holds a record, so it never fires unless the rule above is broken.
6. **Without a skipped field**, and with skipped fields whose collections have no pending record, the open is exactly
   the writer's, as item 5 says: its replay applies the other collections' records and may flush them and retire a
   cut commit.

Consequences: item 2's "the refusal comes from the open's replay, when a replay-time flush is due" and item 4's "the
replayed records of a skipped collection are never flushed, so they stay in the WAL" now read: no record of a skipped
collection is replayed by an accepted open, and a refused one writes nothing. `persia compact` exits 1 after any crash
(or handle dropped without a commit) of the application that leaves such a record, until the application opens and
closes the database (closing a `persia::Db` commits, flushing every memtable); its hint says so. The trade-off was
accepted: the command refuses more often, and never grows the container while it does.
