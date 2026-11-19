# ADR-0052: Blob garbage collection: the mark, the sweep and the pack rewrite

- Status: proposed
- Date: 2026-11-19
- Related: SPEC §4.6, §8.4, §8.5, §9.3, §9.4, §9.5, §10.1, §18.10; ROADMAP 9.5.1, 9.5.2, 9.5.4, 9.5.7, 9.5.8; ADR-0016,
  ADR-0049, ADR-0050, ADR-0051

## Context

SPEC §9.5 states what GC keeps (the mark: blob ids referenced by live manifests, snapshots and WAL records from
`wal_start`, the pinned ids, then the uploads their manifests name) and how it deletes (manifests before their
upload's chunks, a grace period from `last_modified`, each pin check and delete one step under the pin set's lock, a
per-prefix remote listing). §9.3 says GC rewrites the pack "like container compaction"; §18.10 proposes the reader-safe
replace. Left open: who triggers GC, what the engine exposes to the mark (it must not depend on `persia-blob`), what a
snapshot is for the mark, whether the pack has a grace period, when a rewrite is worth it, where the chunks of
uploads in flight go in a new pack, and how the sweep meets ADR-0051 item 5 (a pinned blob's manifest keeps its
upload). The user decided (2026-11-19): an explicit `gc` only; grace 24 h by default and never below the bind budget;
a rewrite past a ratio of dead bytes; the writer's view as the snapshot set; no grace locally; unknown ages kept.

## Decision

1. **Entry point.** `persia::internal::BlobDatabase::gc(&GcOptions) -> Result<GcReport, GcError>`, called by the
   writer, never in the background. `GcOptions { grace (24 h), rewrite_ratio (0.5), now (None: the system clock) }`.
   A grace below `DEFAULT_BIND_BUDGET` (60 s) is `GraceBelowBindBudget` (SPEC §9.4: a conditional write may still land
   that long after `put_blob` gave up); a negative or NaN ratio is `InvalidRewriteRatio`. `gc` borrows the database
   shared: no document can be logged, deleted or committed while it runs (`put` and `commit` take `&mut`), while
   `put_blob` and reads may go on in other threads. No state is kept between calls.
2. **The engine's side of the mark** is `Database::referenced_blobs() -> HashSet<BlobId>` (persia-engine
   `db/blob_refs.rs`, behind `persia`'s `BlobReferences` trait, as `DocumentLog` is): the ids of the documents
   - live in the current state: each segment as its tombstones leave it, each memtable whole (replaced and deleted
     documents too: a view taken before shows them);
   - live in every older collection snapshot a reader in this process still holds, whether through its `Snapshot` or
     a `CollectionSnapshot` cloned out of it: at each publish the writer keeps, per collection of the snapshot it
     replaces, a weak reference to its per-snapshot statistics (shared by every clone, so a lease), its segments with
     their deletions, and its memtable, and reads the segments with those deletions while the lease is alive;
   - of every memtable a reader still holds (a `MemtableView` outlives its snapshot), whole;
   - of a live segment as an older sidecar a reader still holds leaves it (SPEC §8.5 tracks superseded sidecars);
   - of the obsolete segments still held (SPEC §8.5: a reader holding a segment, a merge in flight), deleted ones
     included (a reader may hold one with older deletions);
   - of every WAL put from the committed `wal_start` on, the records after `log_end` included (what a reopen replays and
     `verify` checks), read without their schema.
   A poisoned database refuses (`Poisoned`): its view may not be the disk's. A segment's deleted document counts only
   in the snapshots from before its deletion, so deleting a document frees its blob once no reader holds an older
   snapshot, not only after a merge rewrites the segment.
3. **The mark** (`persia_blob::gc::Mark`): the referenced ids, the pinned ids (taken first; with the log frozen the
   order does not matter), then `add_manifest_uploads` reads each marked blob's manifest through `BlobStore::manifest`
   and adds the upload it names; an inline manifest names none. A marked blob without a manifest is listed in
   `missing` (a dangling reference, for `verify`); one whose manifest cannot be read in `unreadable`; a store error
   fails GC. This crate knows neither the engine nor the pin set: the writer passes the mark and a `PinGate`
   (`pinned()`, `unless_pinned(object, delete)` with a locked `PinCheck` view), which `persia` implements for its
   `PinSet`.
4. **The remote sweep** (`gc/remote.rs`). For each of the 256 `blobs/<xx>/` prefixes: parse every key (one that does
   not parse is ignored); a marked manifest is kept; an unmarked one older than the grace period (`now - last_modified
   > grace`) is deleted under `unless_pinned(Blob)`; a manifest kept for any reason (young, unknown age, pinned at its
   check) is read, so the upload it names keeps its chunks. Then one listing of `blobs/uploads/`: chunks are grouped by
   upload, an upload is aged from its newest chunk, and one that no kept manifest names, old enough, has its chunks
   deleted under `unless_pinned(Upload)`. A `last_modified` of `None` is never taken for expired and is counted
   (`unknown_age`); a time after `now` counts as young. An unreadable manifest, marked or kept, skips the chunk phase
   (`uploads_skipped`): the upload it names is unknown. Manifests go first, so a crash leaves chunks without a
   manifest, never the reverse. The first storage error stops the sweep.
5. **A pinned blob's manifest keeps its upload** (ROADMAP 9.5.8, ADR-0051 item 5). Inside the chunk delete's
   `unless_pinned`, every pinned blob is looked at: a marked one's upload is kept already, a kept manifest's upload was
   learned by the listing, and any other's manifest is read there, under the lock (it may have been bound after the
   listing); one that cannot be read counts as naming the upload. Locally this holds by construction (item 6).
6. **The pack has no grace period.** Only the lock holder writes the pack, local uploads are not resumable
   (ADR-0049), and the pins cover every `put_blob` in flight, so what nothing keeps is dropped at once: a blob is kept
   when it is marked or pinned or its upload is marked or pinned (pins are read again under the writer mutex), and the
   chunks this handle staged when their upload is. A bound blob's chunks go with it, and a staged upload is one no
   manifest names yet, so no pinned manifest can name dropped chunks.
7. **The pack rewrite** (`pack/store/rewrite.rs`, a child of `pack::store`). It runs only when the bytes of the frames
   nothing keeps, over the bytes of all frames (`12288..end`), reach `rewrite_ratio` (0 forces it; above 1 never),
   and holds the writer mutex throughout, so nothing is staged or bound meanwhile. The new file is
   `<pack>.tmp.<nonce>.<n>` (the pattern a read-write open sweeps, ROADMAP 9.2.6, so a crashed rewrite is cleaned up):
   the header, both slots with `generation + 1` and a new random `pack_id` (`seq 1` with the last index frame ending
   at `log_end`, or exactly the new-pack values when no blob is kept), each kept blob's chunk frames then its
   `BlobIndex` frame, renumbered from `frame_seq` 1, each chunk re-checked against its manifest (frame CRC, head, the
   bytes' CRC32C: a rewrite never copies damage forward; a damaged kept frame stops it), then the staged chunks of
   pinned uploads **after `log_end`**, as any staged chunk (a crash drops them, as it would from the old pack). It is
   synced, then, as one step under the pin set's lock (`unless_pinned` of the first object dropped, with a check of
   every other): if a pin taken since the plan names a dropped blob, its upload or a dropped upload, the new file is
   removed and the rewrite reported `Deferred`; otherwise it is renamed over the pack (only while the path still names
   the handle's file), the directory synced, and the handle's file, index, uploads, chunk frames and staged chunks
   swapped at once under the `RwLock`'s write side, the mapped window remapped. A failure before the rename leaves the
   pack as it was. A failed directory sync poisons the handle, which moves to the new file (SPEC §18.10 (b): a blob
   bound into it could be lost with the rename by a crash while documents reference it); so does a rename that
   reported an error yet happened (the path no longer names the handle's file). Never in place.
8. **Readers.** In this process a `get_blob` stream pins its blob and reads each chunk by id, so it reads on in the new
   file; a `get_blob_range` pins for the call (§18.10 (c)). In another process a reader keeps the old file and its
   bytes, and `refresh` reports `PackReplaced`. Remotely, readers in other processes are not in the writer's view: one
   that lags behind may find the blob of a document the writer deleted gone once the grace period has passed (ROADMAP
   M12/M13 follow-up proposed with this ADR).
9. **Tests.** Model-based proptests of `referenced_blobs` and of both sweeps (exactly what the mark, pins and grace
   keep; unknown ages; foreign keys; the ratio); crash tests at every call of the remote sweep (`LocalStorage<SimFs>`
   with a clock, ROADMAP 9.5.7) and of the rewrite (`SimFs`): the pack reopens old or new, never a mix, with every kept
   blob; the failed directory sync; a proptest of GC against documents (`persia`, real files). `SimFs::with_clock`
   gives simulated files modification times (stamped at creation, write and length change; kept across renames,
   links and crashes), which the local backend lists as `last_modified`.

## Consequences

- A GC reads every live segment's `BlobRefs` (one record per document), the WAL from `wal_start`, and, remotely, every
  kept manifest (one `get` each) plus 257 listings, the last of all staged chunks. Memory is one id per referenced
  blob and one entry per unkept upload's chunk. Fine for v1; a remote listing that streams pages, and parallel manifest
  reads, are later work.
- The pack rewrite copies every kept byte while holding the writer mutex: `put_blob` waits for it. The ratio keeps it
  rare; an incremental rewrite is not planned.
- A reader that keeps only an `Arc<Segment>` of a live segment, dropping the snapshot and the deletions it read with,
  reads deleted documents as live, and is not seen: a reader must keep the collection snapshot or the deletions.
- Remotely an upload's chunks are deleted in one step under the pin set's lock, not one chunk per step: a pin taken
  midway (a blob bound late that names the upload) must find the upload whole. The lock is held for one request per
  chunk of that upload.
- `grace` is checked against `DEFAULT_BIND_BUDGET`, not against the budget a caller passed to `put_blob`
  (`PutBlobOptions::bind_budget` has no upper bound): a caller who raises the budget must raise the grace with it.
  Open: cap the budget, or record the largest one used.
- Holding the pin set's lock across the rename and the directory sync blocks new pins (a `put_blob`'s first pin, a
  read's) for that long, as a remote delete does under SPEC §9.5.
- A segment's deleted documents stop protecting their blobs once no held snapshot predates the deletion; `verify`
  (ROADMAP 9.5.3) must check live documents only, or it would report blobs GC rightly collected.
- Remote sweeps trust the backend's clock and `now`; a writer whose clock runs far ahead of the store's could expire
  objects early. The grace period (24 h) dwarfs ordinary skew.

## Alternatives considered

- **Background GC** (a timer in the writer): rejected by the user for now; the embedding calls `gc`.
- **Marking every document of every live segment, deleted ones included**: simpler, but a deleted document's blob
  would stay until a merge or a deletion rewrite, which may never come for a large segment.
- **Keeping the pinned staged chunks below `log_end`** (or deferring the rewrite when some exist without a live blob):
  a committed slot's last frame must be a `BlobIndex` and `seq 0` means exactly the new-pack values (SPEC §9.3); after
  `log_end` they are where any staged chunk is, and a crash drops them either way.
- **A grace period locally**: needless (item 6), and it would keep the pack large for a day after every delete.
- **`<pack>.compact.<nonce>.<n>`** (§18.10's first wording): the user chose the creation's `.tmp.` pattern so the
  read-write open's sweep (ROADMAP 9.2.6) cleans a crashed rewrite too.
- **Sweeping in `persia`** with the pin set directly: the remote listing and the pack's internals belong to this
  crate; the `PinGate` trait keeps the pin set where its other users are (ADR-0051).
