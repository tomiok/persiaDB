# ADR-0041: A writer's blob chunk size has a 64 KiB floor; the encodings keep `1..=64 MiB`

- Status: accepted
- Date: 2026-11-13
- Related: SPEC §9.1, §9.1.1, §9.2, §9.4; ROADMAP 9.4.2, 9.4.4; amends ADR-0035 (Consequences)

## Context

ADR-0035's Consequences note that a blob manifest has 4 bytes per chunk, so the largest one (`u32::MAX` chunks) is
16 GiB, which only a pathological `chunk_size` reaches: `chunk_size` 1 gives a 4 GiB blob a 16 GiB manifest.
ROADMAP 9.4.4 asks to decide, before the manifest encoding is released (SPEC §15), whether `put_blob`'s chunk size
gets a floor or the manifest a size cap.

## Decision

1. **A floor on the writer, not a cap on the manifest.** A writer's `chunk_size` is `64 KiB..=64 MiB`, default
   4 MiB. With the floor a manifest this writer writes is at most `76 + 4·ceil(size / 64 KiB)` bytes: 256 KiB for a
   4 GiB blob. A blob still holds at most `u32::MAX` chunks: about 256 TiB at the floor.
2. **The encodings keep `1..=64 MiB`.** `BlobRef` (ADR-0011) and the blob manifest (ADR-0035) are unchanged, and so
   are their golden files (`three.blm` has `chunk_size` 5). A reader accepts every chunk size the encodings allow, so
   no stored value becomes invalid, and the floor can move later without a format change.
3. **Where it is checked.** `ChunkSize::new` (a writer's constructor) rejects a size below `ChunkSize::MIN` with
   `Error::ChunkSizeBelowMin`, a caller's error, not corruption. `ChunkSize::from_encoding` is the reader's
   constructor (`1..=64 MiB`). The codec primitives (`Chunker`, `chunk_all`, `BlobManifest::new`) take any
   `ChunkSize`: they also serve stored sizes (re-chunking a stored blob to verify it, fixtures). `put_blob`
   (ROADMAP 9.4.2) rejects a size below the floor however its `ChunkSize` was built, a resumed upload's token
   included (SPEC §9.2, §9.4).

## Consequences

- No blob this database writes needs a manifest above 4 bytes per 64 KiB of blob.
- A manifest from another writer may still be up to 16 GiB. Decoding allocates at most its length, and a remote
  reader may check its length against `76 + 4·n` before it fetches it (SPEC §9.1.1).
- A non-empty blob smaller than the chunk size it is written with is still one chunk. An empty blob has
  no chunks (`chunker.go`). The floor applies to the requested `chunk_size`, not to the blob's length: a
  request below 64 KiB is rejected even when the blob itself is smaller than 64 KiB, including an empty one.
- A breaking change of `persia-blob` (unreleased): `ChunkSize::new` rejects `1..64 KiB`, which it used to accept.

## Alternatives considered

- **A manifest size cap in the encoding** (a reader rejects a manifest above N bytes): a format rule, so a format
  change to move it. The cap limits the encoded chunk list, not blob bytes. Rejected.
- **A floor in the encoding** (a reader rejects `chunk_size` below 64 KiB): turns stored values invalid and the golden
  files with them, for no reader-side gain over the writer's floor. Rejected.
- **The floor in the codec primitives too**: would forbid them the stored sizes they must read. Rejected.
