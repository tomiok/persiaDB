# ADR-0035: Blob manifest encoding

- Status: accepted (amended by ADR-0041)
- Date: 2026-11-07
- Related: SPEC §9.1.1, §9.2, §9.3, §9.4, §15; ROADMAP 9.1.3, 9.2.1, 9.3.1, 9.3.3; ADR-0002, ADR-0024, ADR-0030, ADR-0034

## Context

ADR-0034 binds a verified `blob_id` to the chunks staged under an upload id with a *blob manifest*: `blob_id`,
`size`, `chunk_size`, `upload_id` and one CRC32C per chunk. It is stored locally inside a `BlobIndex` frame and
remotely as the object `blobs/<xx>/<id>`, which is not inside a frame. It is read on every blob read, by
`verify`, and by GC's mark phase, and a remote one comes from storage Persia does not control, so it is hostile input
(SPEC §17). It needs a byte layout before the pack file (ROADMAP 9.2) and the remote layout (9.3) can use it.

## Decision

1. **Layout** (SPEC §9.1.1), little-endian, fixed-width like the tombstone sidecar (ADR-0030): magic `"PBLM"`,
   encoding `version u32 = 1`, `flags u32`, `blob_id` (32 bytes), `size u64`, `chunk_size u32`, `upload_id u128`,
   one `u32` CRC32C per chunk, and a CRC32C trailer over everything before it. The header is 72 bytes, so the CRC list
   starts 8-aligned; the length is `76 + 4·n` with `n = ceil(size / chunk_size)` for version 1 without flags, a sanity
   bound a reader can use once it has read the version and flags (it fetches the whole object). The magic starts and
   the trailer ends every version and flag layout.
2. **Chunk lengths are not stored.** Chunks are fixed-size (ADR-0034 item 1): every chunk's range follows from `size`
   and `chunk_size`. Storing them would only add states to reject.
3. **Own trailer.** A remote blob manifest is a bare object, so it carries its CRC32C; inside a frame the trailer is
   redundant, as for manifests and sidecars.
4. **Version and flags.** The `version` field versions this encoding alone (SPEC §15: encodings inside frames or
   objects); 0 is corruption, above 1 unsupported. `flags` is reserved for ROADMAP 9.3.3's inline small-blob form
   (ADR-0034 item 8): no bit is assigned, and a bit this build does not know is *unsupported*, not corruption,
   because it means a newer writer's layout. Assigning one is a SPEC change. The container format version does not
   change: no existing structure changes, and the pack file (ROADMAP 9.2.1) is new.
5. **Validation order:** length ≥ 76, magic, trailer, version (0 is corruption, above 1 unsupported), flags (a bit
   this build does not know is unsupported, item 4), then `chunk_size` in `1..=64 MiB`, `n` fits in a `u32`, exactly
   `4·n` CRC bytes before the trailer (checked before allocating), and an empty blob only with the SHA-256 of no
   bytes as its id. Every other violation is corruption. Fields are fixed-width and a v1 reader accepts only `flags`
   0, so each value it accepts has exactly one encoding.
6. **Lookup checks.** A manifest's checksums cover only its own bytes, so a reader checks it against what it looked it
   up by: `check_blob_id` (the id of its key or `BlobIndex` entry) and `check_ref` (id, size and chunk size of the
   `BlobRef`), each a mismatch `Corrupt` (SPEC §9.3, §9.4).
7. **Golden files** live in `tests/fixtures/blob-manifest/v1/`, checked by `persia-blob`'s tests and written when
   missing with `PERSIA_WRITE_BLOB_MANIFEST_FIXTURES=1`, as for the other object encodings (SPEC §15). `cargo xtask
   fixtures` cannot write them: `xtask` may depend on `persia-format` only (CLAUDE.md "Architecture"), and the codec
   belongs to `persia-blob`.

## Consequences

- A 1 GiB blob in 4 MiB chunks has a 1 100-byte manifest; the largest one (`u32::MAX` chunks) is 16 GiB, which only a
  pathological `chunk_size` reaches, and decoding allocates at most its input's length.
- A reader cannot verify a manifest against the blob's bytes without reading them; `verify` (ROADMAP 9.5.3) re-hashes.
- The codec lives in `persia-blob`, not `persia-format`: the engine never reads blob manifests, only `BlobRef`s.

## Alternatives considered

- **Varints** (as `BlobRef`, ADR-0011): saves a few bytes per blob, but a manifest is one per blob and is not
  embedded in a column, and fixed width lets a reader compute the length up front.
- **Per-chunk length or offset:** needed only for variable-size chunks, which ADR-0034 rejected.
- **A `PFR1` frame as the remote object:** reuses frame validation, but adds a 32-byte header whose `frame_seq` has no
  meaning outside a container.
