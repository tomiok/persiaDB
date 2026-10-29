# ADR-0032: Background merges, and obsolete objects freed by reference counting

- Status: proposed
- Date: 2026-10-28
- Related: SPEC §4.5, §4.6, §5.5, §7.2, §8.3, §8.5, §8.7.2, §14; ROADMAP 6.5.3–6.5.5; ADR-0029 (amended), ADR-0031

## Context

ROADMAP 6.5.1–6.5.2 gave a merge policy and a merger, but nothing ran them. SPEC §8.5 said only that merges run in
background threads, are crash-safe and never block reads or writes, and that obsolete segments are deleted once no
snapshot references them "(reference counting / epoch)". ADR-0029 kept the engine free of threads of its own (the
embedding calls `tick()`), the merged segment had to be verified before its commit (ADR-0031 item 9), and a merge
planned against one manifest may finish after commits that deleted from its inputs.

## Decision

1. **The writer plans and commits; workers only merge.** `Database::merge_step` commits the jobs that finished, then
   plans with `MergePolicy` and submits jobs; `finish_merges` runs until the policy plans none; `cancel_merges` stops
   every job in flight. A job holds its inputs as the writer's `Arc<Segment>`s and `Arc<Tombstones>` at the plan's
   sidecar versions (segments are immutable), so a worker needs no lock and no container. It merges, writes the
   merged bytes, opens them and runs `Segment::verify`, in memory. The engine starts threads for merges only, since
   SPEC §8.5 requires merges off the writer's thread; the embedding still drives every commit, as with `tick()`.
2. **`std::thread` pool, `merge_threads` workers** (default 1, SPEC §14), started at the first job and joined when
   the database is dropped (after cancelling). `merge_threads` 0 runs one merge per step inline: deterministic for
   tests and for embeddings without threads. A worker's result waits for the next step, so a step never commits a
   merge it started, which makes interleavings testable.
3. **Backpressure is a bound on jobs in flight**: one per thread, counting finished jobs not yet collected, so at
   most `merge_threads` merged segments are held in memory. There is no queue of plans: a plan that finds no room is
   simply made again at a later step from the then-current manifest.
4. **A merge commit applies only to unchanged inputs.** Before writing anything, the writer checks that every input
   is still live at the sidecar version the plan read and that the schema is the one merged with; otherwise the
   result is discarded (`MergeOutcome::Conflict`), and the segments are planned again. This is SPEC §8.7.2's rule,
   checked early so a discarded merge leaves no frame behind. The commit is the usual protocol (SPEC §4.5): segment
   frame, WAL commit point, sync, manifest, slot. Re-applying the newer deletions to the merged segment instead
   would save the merge, but needs a docid map kept with the result; it can come later without a format change.
5. **Cancellation** is a flag per job, checked by `merge::merge_until` before each input and each field, and between
   writing and verifying. Writing, opening and verifying the merged segment are not interrupted, so a cancel (and
   dropping the database) waits at most for those stages of the merges in flight. Cancelled and conflicting merges
   are not failures.
6. **A failed merge is not retried** until the database is reopened (its inputs are left out of planning): damaged
   input fails the same way every time, and retrying would burn a thread forever. The failure is reported as
   `MergeOutcome::Failed`; the database stays usable.
7. **Obsolete objects are freed by reference counting, not epochs.** When a commit stops listing a segment or a
   sidecar, the writer records it with a `Weak` reference to what readers hold of it. Every snapshot, job and
   reader that can read it holds a strong one, so when the weak reference no longer upgrades, nothing can reach the
   object and it may be deleted (`Database::take_unreferenced`; `Database::obsolete` lists them all). A per-snapshot
   epoch would hold back every object retired after the oldest snapshot still held, even ones that snapshot never
   referenced; the counts are already there.
8. **Locally nothing is deleted yet.** Obsolete objects are frames, which a container compaction (SPEC §4.6) reclaims;
   it must keep the frames still referenced. Wiring compaction into the engine is ROADMAP 6.5.8. Remote layouts
   (M12) delete objects from the same list.
9. **The equivalence property (6.5.5) is checked on what a search reads**, as M7 has no query engine yet: per term,
   each live document's id, frequency, positions and norm, and a BM25 score over collection-level statistics (SPEC
   §7.2) counted over live documents, before and after each merge. M7's differential tests extend it to real
   queries.

## Consequences

- A busy writer that deletes from the inputs of a long merge may discard it repeatedly; the merge is lost work, not
  lost data. Item 4's alternative fixes that if it shows up.
- Merged bytes are held in memory until the writer's next step: `merge_threads` × the merged segment size, the same
  bound as a flush per thread (ADR-0031 item 8; ROADMAP 6.5.6 streams it).
- The engine now owns threads, but only for merges, and they never touch the container.
- Obsolete objects of a database nobody calls `take_unreferenced` on accumulate in memory, a few dozen bytes each.

## Alternatives considered

- **Rayon's global pool**: shared with the embedding's own work and with search (SPEC §7.3), so a long merge could
  starve queries; a dedicated, sized pool is simpler to reason about.
- **Workers that commit**: they would need the writer, which is single (SPEC §8.4); handing the result back keeps
  one writer and makes the commit order the writer's.
- **An epoch per snapshot** (SPEC §8.5 named both): see item 7.
- **Retrying failed merges with backoff**: damage does not go away; a reopen (after repair) retries.
- **The facade owns the pool** (ADR-0029's rule that the engine starts no thread): `MergeJob::run` is pure, so
  `persia` could run it. Rejected because every embedding of the engine (the facade, the server, the CLI's
  `compact`, tests) would then reimplement the same pool, bound and cancellation, and SPEC §8.5 asks for background
  merges of the engine. ADR-0029 is amended: the engine runs merge workers, which only read immutable segments; every
  commit still happens on the caller's thread.
