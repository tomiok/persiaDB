# ADR-0031: Tiered merge policy, and a merger that fills a segment builder

- Status: proposed
- Date: 2026-10-28
- Related: SPEC §5.5, §8.3, §8.5, §8.7.2, §14; ROADMAP 6.4.6, 6.5.1, 6.5.2; ADR-0024, ADR-0027, ADR-0030

## Context

SPEC §8.5 named a tiered policy (factor 10, at least 8 segments per tier, or a deleted ratio above 20%) but not what
a segment's size is, where tier 0 starts, how many segments one merge takes, how large a merge may grow, or what
happens to a segment whose documents are all deleted (ADR-0030 item 8 keeps it listed until "a merge"). The merger
(ROADMAP 6.5.2) also had no plan for how its output is written: the only writer, `SegmentWriter`, encodes a
`SegmentBuilder`.

## Decision

1. **The policy is a pure function** of a collection's manifest entries and the set of segments running merges hold
   (like `FlushPolicy` and `CommitPolicy`), so it is table-tested without I/O and the scheduler (6.5.3) stays thin.
   It returns disjoint plans; each lists its inputs in manifest order with the sidecar version it read, which is what
   the merge's diff removes (SPEC §8.7.2), so a deletion committed meanwhile makes the diff conflict instead of
   reviving documents.
2. **Size is the live size**, `size × live / doc_count`: the manifest's byte length scaled by the live documents. It
   needs no segment read (ADR-0024 keeps `deleted` in the manifest for this) and makes a segment with many deletions
   fall to a lower tier, where it merges sooner.
3. **Tiers** start at `tier_floor` (2 MiB): everything below `tier_floor × tier_factor` is tier 0, so the many small
   flushes of a trickle of writes merge together instead of each forming its own tier.
4. **A tier merge takes exactly `segments_per_merge` (8) segments, the smallest first**, and only within the caps
   (`max_merged_bytes` 5 GiB of live size, `max_merged_docs` 2^31 documents). Smallest first keeps write
   amplification low; a fixed count keeps plans predictable. Segments at or above the byte cap take part only in
   deletion rewrites, so huge segments are not rewritten again and again.
5. **Deletion rewrites** take a segment alone once more than `deleted_percent` (20%) of its documents are deleted,
   unless a tier merge already holds it.
6. **Fully deleted segments are dropped**, all in one plan that writes nothing (closes ROADMAP 6.4.6 for the
   background path; the commit itself still keeps them listed).
7. **Order of documents and segments**: a merged segment holds its inputs' live documents in plan order and is added
   after the kept segments. Merges may take segments that are not adjacent: a commit leaves at most one live copy of
   each id (SPEC §8.3) and a merged segment holds only live documents, so the newest-segment-decides lookup gives
   the same answer.
8. **The merger fills a `SegmentBuilder`** from the input readers: term dictionaries merged k-way per field,
   postings and positions copied with docids remapped, norms, columns, `DocStore` rows, ids and blob refs copied, and
   statistics recomputed from what was copied. `SegmentWriter` then writes it, as for a flush. Peak memory is about
   the merged segment's size, the same bound a flush has. A streaming writer is later work (ROADMAP 6.5.6).

## Consequences

- Every knob is in SPEC §14 as `merge.*`; values that would loop or divide by zero are raised to the smallest that
  makes sense, so the function never fails.
- Live sizes are estimates (deleted documents need not be the average size), so a merge may write somewhat more
  than `max_merged_bytes`.
- The first merger copies more slowly than a flush analyzes (`docs/benchmarks.md`, segment merging): most of its time
  is the query-oriented postings cursor, set up per term. Copying stays the design, for the rows it must keep as
  they are (Alternatives), and ROADMAP 6.5.7 gives it a reader of whole lists.
- The policy never merges across collections or schemas; inputs of one collection may differ in
  `schema_field_count` (fields added later), and the merger writes the output under the collection's current schema.

## Alternatives considered

- **Lucene's scored `TieredMergePolicy`** (pick the candidate set with the best skew and reclaimed-deletes score):
  fewer bytes merged over time, but harder to state in SPEC and to test exhaustively. It can replace this function
  later without a format change, since the policy is not on disk.
- **Raw `size` instead of live size**: a segment that is 90% deleted would stay in its tier and merge late.
- **Adjacent-only merges** (to keep segment order meaningful): unnecessary, item 7.
- **Re-indexing stored documents** through `SegmentBuilder::add`: simpler, but it re-analyzes every text field and
  rejects rows that a later `strict` schema would not accept, which a merge must copy as they are (ADR-0023).
