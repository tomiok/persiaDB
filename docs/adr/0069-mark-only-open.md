# ADR-0069: A mark-only open: GC's mark and sweep from the compaction-only open's recovery

- Status: proposed
- Date: 2026-11-26
- Related: SPEC §8.6, §9.5; ROADMAP 10.4.5, 10.4.7, 10.4.4; ADR-0052, ADR-0053, ADR-0054, ADR-0068

## Context

`persia gc` (ROADMAP 10.4.5) runs the blob GC of SPEC §9.5 from the command line. GC runs in the writer: the mark
needs the writer's view of every reference, and the sweep and the pack rewrite need the pack's writer lock, which
only a holder of the container's writer lock can take (`WriterProof`, ADR-0053). The command opened the database
with `BlobDatabase::open`, so with `Database::open`, which needs every analyzer the schemas name (SPEC §6.2.2). With
the built-in analyzers only, a database whose schema names an analyzer registered by its application failed with
`UnregisteredAnalyzer` (exit status 1), as `persia compact` did before ADR-0068.

ADR-0068's `CompactOnlyDatabase` is the writer's open with stand-ins for the missing analyzers, refusing any flush of
what they indexed. It offers only a compaction: no mark (`Database::referenced_blobs`), no committed seq for the
reader leases, no writer proof for the pack.

The question is whether GC is safe on such an open. What GC reads and writes:

- **The mark** (`Database::referenced_blobs`, ADR-0052) lists the `blob_id`s of the documents of the live segments
  (as their committed tombstones leave them), of the memtables whole, of the views readers of this process hold, of
  the obsolete segments still held, and of every put of the WAL chunk frames from the committed `wal_start` on, those
  after `log_end` included. None of it reads a term: a document's blob reference is stored with the document, and the
  WAL puts are decoded from the frames. A stand-in analyzer changes only the postings of the field it stands in for,
  never the documents a memtable holds.
- **The records of a collection whose analysis is skipped** are replayed into its memtable (whole documents) and are
  never flushed (ADR-0068 item 2): they stay WAL records, from `wal_start` on, since the writer's WAL positions keep
  `wal_start` at the oldest chunk holding a record of an unflushed collection (SPEC §8.2.1). So the mark sees each of
  their references twice, in the memtable and in the WAL, and no live blob of theirs is swept.
- **The sweep and the pack rewrite** (SPEC §9.3, §9.5) write only the pack, under its writer lock, after the reader
  leases are judged against the committed seq. They read no document and need no analyzer.
- **The open's own writes** are those ADR-0068 item 3 lists: the tail repair, the replay-time flushes of collections
  without a skipped field, the commit that retires a cut commit (which flushes nothing). A flush that would turn a
  stand-in memtable into a segment is refused before any segment is built or id taken (`DatabaseError::AnalysisSkipped`).

## Decision

1. **`persia_engine::db::MarkOnlyDatabase`**: `open(path, options)` and `open_container(container, options)` run
   exactly the open of `CompactOnlyDatabase` (one shared function: the read-write check and
   `Database::recover_as(.., Analysis::SkipUnregistered)`), so the refusal of ADR-0068 item 2 is the same code, the
   guard inside the engine's flush planning (`check_no_skipped_flush`), not a copy.
2. **Only a mark.** The type offers `referenced_blobs` (the mark's references, `Database::referenced_blobs`), `seq`
   (the committed seq reader leases are judged against), `writer_proof` (to open the pack read-write, ADR-0053),
   `container_path` (where the pack is, beside the container) and `skipped_analysis`. Nothing that writes a document,
   commits, merges, compacts or changes a schema: by type, a GC through it writes no analysis-dependent byte.
3. **The internal layer** (`persia::internal`): `BlobDatabase::open_mark_only(path, options)` opens it with its pack
   (the pack's writer lock, as `BlobDatabase::open`). `BlobReferences` is implemented for it, and `BlobDatabase::mark`
   and `BlobDatabase::gc` need only `BlobReferences` (they never needed `DocumentLog`): a `BlobDatabase` over a
   `MarkOnlyDatabase` collects, and cannot put, store a blob or commit, since those need `DocumentLog`, which it does
   not implement. Its blob store (`BlobDatabase::blobs`) is behind `DocumentLog` too: through it a blob could be
   stored; `mark` and `gc` reach the store inside the module.
4. **`persia gc` always opens with it** (with the built-in analyzers). With every analyzer present nothing is
   skipped and it is the writer's open, so the command's statuses and output are unchanged for such a database. A
   database whose schema names a registered analyzer is collected, unless the open's replay would flush any
   collection while a skipped collection holds a replayed record: then the open fails with `AnalysisSkipped`
   (status 1), with the files as ADR-0068 item 2 leaves them, and nothing is marked or swept.
5. **Crash safety.** GC's own writes are unchanged (SPEC §9.5: a manifest deleted before its chunks; the pack
   rewrite written aside, synced, renamed, the directory synced). A crash in the open leaves what a crash of a
   writer's open leaves; a crash in the GC, what a crash of `BlobDatabase::gc` leaves. The skipped collections'
   records were never flushed, so the next writer, with the real analyzer, replays and indexes them.

## Consequences

- `persia gc` collects a database whose schemas name registered analyzers (ROADMAP 10.4.7). The refusal is the same
  as `persia compact`'s, and so is the way out: open the database with its application and commit, then collect.
- The engine's open contract gains an entry point (`MarkOnlyDatabase`); `Database::open`, `CompactOnlyDatabase` and
  `ReadOnlyDatabase` are unchanged. No on-disk format changes.
- Rejected: **adding the mark to `CompactOnlyDatabase`**. One type would then hold a compaction and a pack writer
  proof; keeping each tool's handle to what it needs keeps "only a compaction" (ADR-0068 item 3) true by type.
  Rejected: **a read-only open for the mark** (`ReadOnlyDatabase::open_skipping_analysis`): a read-only handle takes
  no writer lock, so a writer could log a reference between the mark and the sweep, and the pack's writer lock needs
  the container's (ADR-0053).
