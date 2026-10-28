# ADR-0029: Local flush, commit and snapshot publication

- Status: proposed
- Date: 2026-10-28
- Related: SPEC §4.5, §8.2, §8.2.1, §8.4, §8.7.1, §10.1, §14, §15; ROADMAP 6.3.1–6.3.10; ADR-0014, ADR-0024, ADR-0028

## Context

M6 had the pieces of the write path (memtable, WAL, manifest diffs, the container's commit) but nothing joined them:
no flush into a segment frame, no commit of a manifest naming it, nothing readers could load, no automatic commits.
ADR-0028 left a prerequisite open: the write path must log only what replay can apply (SPEC §8.2.1). And the engine
reads I/O through the `Storage` trait, while a container stores frames without names.

## Decision

1. **One writer object, `persia_engine::db::Database`**, over a container (generic `ContainerFile`, so tests use the
   simulated file). It owns the `WalWriter` (which owns the container), the committed manifest, and per collection the
   schema, the open segments and the memtable.
2. **Flush = commit.** `commit()` flushes every memtable with operations into one segment (its live documents, in
   docid order, re-indexed by the segment builder) and commits them in one manifest. A memtable at its size threshold
   is flushed before the next write to it; `tick()` applies the idle trigger and `commit_interval` (default 60 s) as a
   pure `CommitPolicy` over an injected `Clock`. The engine starts no thread: the embedding (M10) calls `tick()`.
3. **Commit order** (SPEC §4.5). Build every segment first (no I/O; an error leaves the database unchanged). Then
   append the Segment frames, mark the flushed collections in the WAL writer, write and sync the buffered WAL records
   (`commit_point`), sync the frames, append the Manifest frame, write the slot. Any error after the first append
   poisons the database: what reached the disk is known only after a reopen. Creating a database commits an empty
   manifest (`seq` 1); creating a collection is a commit of its own.
4. **Stage, log, publish** (ROADMAP 6.3.9). The memtable splits an operation into a stage (every check, the
   indexing, the id), the caller's log call, and a publish that cannot fail. Rejected operations are never logged,
   and no view sees an unlogged one. A single operation is logged with its durability before it is published; in a
   batch, all but the last are logged `Buffered`. The record's payload is the memtable's own `FieldSet::All`
   encoding, so a put is encoded once.
5. **Schema changes flush every memtable first** (ROADMAP 6.3.10). A logged put of a non-`strict` collection may hold
   extra fields that fail validation once the collection becomes `strict`, and replay decodes with the committed
   schema. Flushing everything makes the change's `wal_start` the end of the log, as for a drop (ADR-0028). The
   schema-change and drop APIs come with that leaf.
6. **Snapshots via `arc-swap`.** After the slot is durable the writer stores a new `Snapshot` (manifest, per collection
   the schema, `Arc<Segment>`s and a `MemtableReader`); readers `load_full` it without locks. Segments are shared
   between snapshots; a flushed memtable lives on in the snapshots that hold it.
7. **The container as `Storage`: a read-only view built from the manifest** (ROADMAP 6.3.6). `ContainerObjects`
   names committed frames with the remote layout's keys (SPEC §10.1, §10.4) and checks every frame against its
   location (kind, length, below `log_end`). Writes are `Unsupported`: a commit is a sequence of appends plus a slot
   write, which no single-object `put` expresses. Segments are opened through it, at open and after each commit.
8. **Checks on open** (ROADMAP 6.3.7): the manifest's `seq`, `fence_token`, `wal_start` against the slot; the layout
   (frame locations only locally, `Manifest::check_layout`); each frame through the view; each segment footer's id and
   document count against its entry.
9. **Segment ids** come from `SegmentIds`: SipHash keyed by the standard library's OS-seeded `RandomState` over a
   counter (no new dependency), or a seeded generator for tests and golden files. A drawn id already in the manifest
   is drawn again.
10. **Golden database** `tests/fixtures/database/v1.0/blob-ref.persia`, written by the commit path with a fixed clock,
    ids and `db_uuid`, compared byte for byte by an engine test (SPEC §15).

## Consequences

- Until ROADMAP 6.4.3, a flush keeps only the memtable's live documents: a delete or upsert of an id that lives in an
  older segment is lost from the flush (the older segment's version shows again). Nothing reads across segments yet;
  6.4.3 adds tombstones through the commit.
- A commit re-indexes the flushed documents: a flush costs about one more analysis pass than reusing the memtable's
  postings would. Reusing them is a possible optimization (`Inverted` is shared with the builder).
- A reader may see a document of a batch before the batch's sync; if that sync fails, the database is poisoned and a
  reopen may not have the document.
- A read-write open on a torn multi-sector append can still fail with `CorruptTail` (ADR-0014): callers pass a
  container opened with `Repair::Quarantine` to `Database::open_container`.

## Alternatives considered

- **Apply to the memtable first, log after** (an unlogged write is briefly visible, and a failed log must be undone),
  or **a dry run before the WAL** (analysis twice). Staging gets both properties for one analysis.
- **A writable `Storage` view** mapping `put` to an append and `put_if_absent` of a manifest to a commit: it would hide
  the step order of SPEC §4.5 behind calls that promise atomic single-object visibility they cannot give.
- **A background flush thread in the engine**: the engine is synchronous by design; the facade owns threads.
- **Replay without re-validation** to survive `strict` changes: a second code path that skips the schema's rules, for
  a rare operation that can afford a flush.
