# ADR-0024: Manifest encoding and commits as diffs

- Status: proposed
- Date: 2026-10-25
- Related: SPEC §4.3, §4.6, §6.2.4, §8.3, §8.7 (§8.7.1–§8.7.2), §10.4, §11.3; ROADMAP 3.3.1–3.3.4, 6.3.2, 6.3.7, 6.4.2

## Context

SPEC named the manifest (the snapshot a commit publishes) and where it lives, locally a Manifest frame (§4.3) and
remotely `manifests/<seq>.mf` (§10.4), but not its contents or bytes. The commit protocol (M6), container compaction
(§4.6, which rewrites the frame offsets a manifest references) and background merges (§8.5, planned against one
manifest and committed on a later one) all need a fixed model, an encoding under the format discipline, and a way to
express and check what one commit changes.

## Decision

1. **Model.** `seq`, `fence_token`, `wal_start`, and per collection the current schema and the live segments, oldest
   first. Each segment entry holds its id, `doc_count`, `size` and location, and its current tombstone sidecar
   (`version`, `deleted` count, location) if it has deletions. ROADMAP 3.3.1 listed `tombstone_versions` as a map
   beside the segments; nesting the sidecar in its segment's entry makes a sidecar of a segment that is not live
   unrepresentable. `deleted` lets the merge policy compute the deleted ratio (§8.5) without reading sidecars;
   `size` lets a remote reader fetch a segment's trailing footer (§5.1) without a `HEAD`.
2. **Locations.** An entry says where its bytes are: an object (the key follows from collection, id and version,
   §10.4) or a container frame (offset and total length, like the superblock's `manifest_off`/`manifest_len`).
   Per entry rather than per manifest, so a replicating writer (§10.5) or a later layout can mix them without a
   new version.
3. **Encoding.** Fixed-width little-endian fields, counts as `u32`, a magic (`PMNF`) so a wrong object is named as
   such, and a CRC32C trailer, since a remote object has no frame CRC around it. The trailer ends every version and
   is checked before `version`, so bit rot in the version field is corruption, not "unsupported". At most 64 MiB:
   bounds what decoding allocates (counts are never used as capacities; every entry costs at least 30 input bytes).
4. **Schemas as JSON.** The full schema (query-time settings included) is stored in its JSON form. Readers accept
   any valid definition, not only the exact text this build writes, so a later release that adds an option with a
   default (and writes it) does not make older manifests unreadable. The price is that a manifest has one encoding
   only up to its schema text; the hash is therefore over the stored bytes. Stored schemas must give every field's
   `id` and every analyzer's version: the JSON defaults would otherwise renumber an evolved schema by name order
   or move a field to the current analyzer version. (Both would also fail the segments' schema hash check, §6.2.5;
   rejecting at decode names the actual problem.)
5. **Commits as diffs** (SPEC §8.7.2). A diff names segments by id: create/drop/change per collection, and within
   a change a replacement schema, removed ids, updated entries and added segments; optional new fence token and
   WAL start. `apply` checks the transition (no regression of fence token, WAL start or tombstones; schema only
   through evolution; ids fresh and unique; `doc_count`/`size` fixed) and gives `seq + 1`. A diff planned on an
   older manifest applies to a newer one exactly when the segments it touches are unchanged, which is the
   conflict check a background merge needs. `diff(a, b)` returns the diff that `apply` turns `a` into `b` with, or
   an error when no single commit does (e.g. an added segment ordered before a kept one).
6. **Golden fixtures** live in `tests/fixtures/manifest/v1/` (not in `tests/fixtures/format/`, which `cargo xtask
   fixtures` owns and which cannot build engine types). An engine test checks they decode to known manifests that
   encode to the same bytes; missing ones are written with `PERSIA_WRITE_MANIFEST_FIXTURES=1`, existing ones never.

## Consequences

- The container reader (ROADMAP 6.3.7) still has to check a local manifest against its slot (`seq`, `fence_token`,
  `wal_start`) and its frame locations against the file; the manifest alone cannot.
- Tombstone sidecar versions start at 1 and step by one, which fixes part of ROADMAP 6.4.2 ahead of its ADR.
- A remote `wal_start` is defined as the `seq` of the first WAL object needed; M12 may refine it with the remote WAL.
- Large schemas (synonyms up to 1 MiB) are repeated in every manifest. Acceptable for v1; a later version could
  reference unchanged schemas by hash.

## Alternatives considered

- **CBOR (ADR-0023's subset) for the whole manifest**: self-describing, but the manifest is a fixed structure, and
  fixed-width fields are simpler to bound and to fuzz.
- **The schema's canonical index definition only** (§6.2.5): too little; the manifest is where ranking, relaxation and
  synonyms live.
- **Require the schema text to be exactly this build's `to_json`**: one encoding per manifest, but any change to the
  JSON writer would turn old manifests into corruption.
- **Full manifests per commit without diffs**: the writer would compare manifests itself to detect merge conflicts;
  the diff makes the rules one function, shared by planning and checking.
