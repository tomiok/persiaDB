# ADR-0030: Tombstone sidecars, and upserts and deletes through the commit

- Status: proposed
- Date: 2026-10-28
- Related: SPEC §5.3.6, §8.2, §8.3, §8.7, §8.7.2, §10.4, §15, §18.3; ROADMAP 6.4.1–6.4.4; ADR-0024, ADR-0029

## Context

Segments are immutable, so a delete or an upsert of a document already in a segment must be recorded beside it.
SPEC §8.3 proposed full-set tombstone sidecars, referenced by the manifest with a version and a deleted count, but
left the byte layout open (SPEC §18.3), and the manifest's diff rules (§8.7.2) left the writer to prove that each
new version keeps every earlier deletion: the manifest sees only counts. Until now (ADR-0029 Consequences) the flush
kept only the memtable's live documents, so `delete` refused an id already in a segment and an upsert left the older
copy live.

## Decision

1. **Full-set sidecars, not copy-on-write footers** (closes SPEC §18.3). A sidecar holds every deleted docid of one
   segment at one version, so one read is enough and older versions are garbage as soon as no snapshot holds them.
2. **Layout** (SPEC §8.3), little-endian:
   `"PTMB"`, encoding version `u32 = 1`, `segment_id u128`, sidecar version `u64`, `doc_count u32`, `deleted u32`, the
   deleted docids as a portable-format roaring bitmap, and a CRC32C trailer over everything before it. The trailer is
   redundant inside a frame but not in a remote object, like the manifest's and the WAL records' (ADR-0024, ADR-0028).
   Reading checks length, magic, CRC, then the encoding version (0 is corruption, above 1 unsupported), then: version
   ≥ 1, `1 ≤ doc_count ≤ 2^31`, `1 ≤ deleted ≤ doc_count`, a bitmap that deserializes and ends exactly at the trailer,
   whose cardinality is `deleted` and whose docids are below `doc_count`. Opening a database also checks the sidecar
   against its manifest entry: `segment_id`, version, `doc_count` and `deleted`.
3. **The bitmap is not canonical.** The portable format leaves container offsets and container kinds free; the reader
   accepts any valid form, the writer runs `optimize()` (run containers where they are smaller, so a bulk delete
   stays small). Nothing hashes a sidecar, so one encoding per value is not needed. The writer's largest sidecar,
   every document of a `2^31`-document segment with each container in its smallest kind (at most 8 KiB), fits
   `MAX_SIDECAR_LEN` (`2^28 + 2^20` bytes); an encoding above it (possible only with needlessly large run
   containers) is corruption.
4. **Superset check before commit.** The writer builds version `n + 1` as the union of version `n` and the new
   deletions, then checks `TombstoneSidecar::check_follows` (the next version, every earlier docid kept) before it
   appends the frame. A failure is an internal error, before anything is written.
5. **Tombstones are written by the flush commit** (SPEC §8.2, ROADMAP 6.4.3). For each memtable it flushes, the
   commit takes every id the memtable has an operation for (a put or a delete, replayed or not) and looks it up in
   every committed segment of the collection; every live copy found
   is deleted in the next sidecar version of its segment, which goes in the diff as an updated entry. A segment's
   sidecars are appended after the new segments and before the WAL's commit point and the manifest (SPEC §4.5
   step 1). Looking in every segment, not only the newest holding the id, also removes duplicates left by builds
   before this change (ADR-0029 Consequences).
6. **Lookups** (ROADMAP 6.4.1): the memtable's version or delete of an id hides the segments'; otherwise the newest
   segment whose `IdMap` holds the id decides: its copy if live, none if deleted (`CollectionSnapshot::lookup`,
   `get`). Older copies are not consulted, so a lookup never depends on every older copy having been deleted.
7. **Visibility.** A write is visible in the memtable once logged (ADR-0029); the commit publishes the new segment and
   the sidecars that hide the older copies in one snapshot, so every snapshot shows each id at most once.
8. A segment whose documents are all deleted stays listed (with `deleted = doc_count`) until a merge drops it
   (ROADMAP 6.5); removing it at once is a possible optimization. *Update (ROADMAP 6.4.6):* the flush commit that
   deletes a segment's last live document now removes it from the manifest instead of writing that sidecar; the
   merge policy's drop plan remains for segments listed so before.
9. **No format version bump.** The sidecar encoding carries its own version (1), frame kind 5 and the manifest's
   sidecar reference already existed, and no build that writes sidecars has been released. Builds before this
   change read a sidecar's frame but ignored its payload, so they would show deleted documents; none was released.
10. **The decoder relies on `roaring`'s checked `deserialize_from`** (sorted keys and values, run and cardinality
    checks, a bounded container count); the unchecked variant panics on hostile bytes. Tests pin that behavior, so a
    `roaring` upgrade that weakened it fails them.

## Consequences

- `delete` of a committed document no longer fails; `DatabaseError::NeedsTombstones` is gone.
- A flush costs one `IdMap` lookup per touched id per committed segment, plain inserts of new ids included (a few
  hundred nanoseconds each; benches `commit_upserts` and `commit_inserts`), plus one sidecar frame per segment that
  lost documents. Merging (6.5) bounds the segment count; pruning by id range (6.4.5) helps time-ordered ids only,
  and a per-segment membership filter (6.4.7, a format change) would help random ones.
- Each rewrite stores the full set again: a segment deleted from in many commits writes sidecars that grow up to its
  bitmap size each time. Merges, triggered by the deleted ratio (SPEC §8.5), bound that.
- The writer holds every live segment's deleted set in memory (shared with readers as `Tombstones`).
- Remotely (M12), a sidecar's key names its segment and version, not its content. A writer that crashes after
  uploading `tombstones/<c>/<id>.<v>.tomb` but before its manifest, and loses unsynced WAL records, may replan the
  same version with other deletions and write the same key with other bytes. Overwriting an unreferenced key is
  safe only if no reader can see it before the manifest does; the remote commit protocol (ROADMAP 12) must decide
  between overwrite-before-commit and a nonce in the key.

## Alternatives considered

- **Delta sidecars** (only the new deletions per version): smaller writes, but a reader must read every version, and
  dropping old versions needs a compaction of the deltas.
- **Copy-on-write segment footers**: rewrites part of a segment object, which breaks immutability and caching.
- **A canonical bitmap encoding** (re-encode and compare): rejects valid bitmaps from other roaring writers for no
  benefit, since no sidecar is hashed or compared by bytes.
- **Looking up only the newest segment holding an id**: cheaper for upserts (inserts must check every segment
  anyway), but an older live copy, from a build before this change or a future bug, would stay live: invisible to
  lookups, which the newest copy decides, but kept by a merge, which drops only tombstoned documents.
