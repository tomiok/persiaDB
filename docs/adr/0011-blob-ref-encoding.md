# ADR-0011: `BlobRef` value limits and binary encoding

- Status: accepted
- Date: 2026-10-12
- Related: SPEC §9.1, §9.2, §5.3, §4.1; ADR-0002; ROADMAP 1.10.1, 3.2.1, 5.2.6, 9.4.3

## Context

SPEC §9.1 named the fields of a blob reference, `{ blob_id, size, mime, chunk_size, user_meta? }`, but not their
types, limits or bytes. The reference is stored per document in the `BlobRefs` column (SPEC §5.3), in DocStore
records and in WAL records, so it needs one canonical binary form before any of those are written. Every byte of
it comes from disk or from clients, so the decoder must bound every length before it allocates.

`user_meta` had no type at all. SPEC §9.2 already puts searchable metadata (mime, filename, tags) in document
fields, so `user_meta` only carries small opaque annotations that travel with the blob (e.g. an upload's original
filename or a client checksum label).

## Decision

- **Types and limits.** `size: u64`; `chunk_size: u32` in `1..=64 MiB`, with `ceil(size / chunk_size) <= u32::MAX`
  so a chunk index is a `u32`; `mime`: 1..=255 bytes of printable ASCII, not normalized; `user_meta`: a map of
  UTF-8 strings with at most 32 entries, keys 1..=128 bytes, values 0..=1024 bytes. An empty map means "absent".
- **Encoding** (SPEC §9.1): `version u8 = 1`, the 32 raw `blob_id` bytes, `size` and `chunk_size` as canonical
  varints, `mime_len u8` + bytes, `meta_count` varint + entries as `key_len` varint, key, `value_len` varint, value.
  Keys are strictly increasing by bytes. Byte-aligned, no padding, no checksum of its own.
- **Validation.** One rule set is checked when a `BlobRef` is built (so encoding cannot fail) and when it is
  decoded (a violation is corruption at the offending field; the chunk-count rule at `chunk_size`). A `version` above
  1 is unsupported, not corrupt: the enclosing checksum passed, so a newer writer produced it. `version` 0 is
  corruption, since no writer produces it and zeroed bytes are a common failure.
- **Rolling out a new version.** Readers decide on container flags, not on minor versions (SPEC §15), so a writer
  emits a new `BlobRef` version only behind an `incompat_flags` bit (or a format major bump). An old reader then
  refuses the container at open instead of failing mid-query or during WAL replay; the per-value byte is a safety
  net and lets one container hold both versions after the flag is set.

## Consequences

- Every value has exactly one encoding, so checksums of enclosing blocks and golden files are stable.
- The largest encoding is 37 295 bytes, so the input a decoder reads per reference is bounded whatever the bytes say;
  what it allocates is that plus `String`/`BTreeMap` overhead.
- 64 MiB chunks cap the memory a chunk-at-a-time reader needs; 4 MiB (the default) stays well inside.
- Changing a limit or a field is a new `version` behind an `incompat_flags` bit (see above).

## Alternatives considered

- **CBOR** (already allowed for DocStore): self-describing but not canonical without extra rules, larger, and the
  `BlobRefs` column wants a compact fixed-order record.
- **Fixed-width fields:** simpler random access, but the column is read per hit, not scanned, and varints save a
  few bytes per reference (more with fixed-width lengths).
- **Opaque bytes for `user_meta`:** no UTF-8 check, but every API (JSON, gRPC, SDKs) would have to pick an
  encoding for them; a string map maps directly onto JSON objects.
- **A CRC per reference:** redundant inside checksummed blocks, frames and WAL records.
