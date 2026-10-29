# ADR-0029: Local flush, commit and snapshot publication

- Status: proposed; amended by ADR-0032 (the engine runs merge worker threads)
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
2. **Flush = commit, of every collection.** A flush commit flushes every memtable with operations into one segment
   each (its live documents, in docid order, re-indexed by the segment builder) and commits them in one manifest,
   whatever triggered it: `commit()`, a memtable at its size threshold (before the next write to it), or `tick()`,
   which applies the idle trigger and `commit_interval` (default 60 s, measured from each memtable's first unflushed
   write) as a pure `CommitPolicy` over an injected `Clock`. The engine starts no thread: the embedding (M10) calls
   `tick()`. Flushing every collection makes `wal_start` reach the end of the log at each flush commit, so a crash
   never replays a record already in a segment, which the next flush would write a second time (found in review).
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
9. **Segment ids** come from `SegmentIds`: the standard library's keyed hasher (`RandomState`, OS-seeded) over a
   counter, a keyed PRF with no new dependency, or a seeded generator for tests and golden files. SPEC §8.7.1 needs
   uniqueness, not unpredictability (std does not specify the hash, and successive `RandomState`s of a thread share
   half the key, which keeps outputs distinct). A drawn id already in the manifest is drawn again.
10. **Golden database** `tests/fixtures/database/v1.0/blob-ref.persia`, written by the commit path with a fixed clock,
    ids and `db_uuid`, compared byte for byte by an engine test (SPEC §15).

## Consequences

- Until ROADMAP 6.4.3, a flush keeps only the memtable's live documents. So `delete` refuses an id that is in a
  committed segment (`NeedsTombstones`, before logging) instead of letting the next flush undo it, and replacing such
  a document leaves the older copy live in its segment (a newest-first lookup finds the new one). 6.4.3 adds
  tombstones through the commit and lifts both.
- Flush commits write a segment per collection with operations, so one busy collection makes small segments of the
  others; merging (ROADMAP 6.5) absorbs them.
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
- **Flushing only the collections that are due**: fewer small segments, but `wal_start` then stays behind the
  oldest unflushed record of another collection, and after a crash replay re-applies records already flushed, which
  the next flush writes a second time until tombstones (6.4.3) hide them.
- **A background flush thread in the engine**: the engine is synchronous by design; the facade owns threads. (ADR-0032 adds merge workers, which never write; commits stay on the caller's thread.)
- **Replay without re-validation** to survive `strict` changes: a second code path that skips the schema's rules, for
  a rare operation that can afford a flush.
