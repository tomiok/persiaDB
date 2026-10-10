# ADR-0005: Block compression codecs and the decompression-bomb guard

- Status: accepted
- Date: 2026-10-10
- Related: SPEC §4.1, §5.2, §17; ROADMAP 1.4.1–1.4.3, 1.5

## Context

SPEC §4.1 says compression is zstd and that every compressed unit declares its codec, but not how the codec is
encoded, what a valid zstd payload is, or how a reader protects itself from a payload that claims (or expands
to) gigabytes. Blocks are read from object storage and must be treated as hostile (SPEC §17). No format version
has been released, so this defines the codec byte rather than changing it.

## Decision

1. **One `u8` codec id**: `0 = None`, `1 = Zstd`. Every other value is corruption, so a future codec (e.g. a
   dictionary-trained zstd) is a new id plus a format version bump, and old readers fail cleanly.
2. **A zstd payload is exactly one zstd frame that declares its content size**, and that size must equal the
   unit's `uncompressed_len`. The frame magic is checked (rejecting skippable and legacy frames), and the frame
   must span the whole payload (rejecting concatenated frames and trailing bytes). One canonical shape keeps
   the decoder small and lets the reader reject a lying header *before* allocating.
3. **The caller bounds every decode** with `max_len` (for blocks: the section's block size). `uncompressed_len`
   above it is `UncompressedTooLarge`; the output buffer is exactly `uncompressed_len` long, so zstd cannot
   write past it; a payload of another length is `UncompressedLenMismatch`.
4. **The zstd level is a writer choice and is not recorded**: any level decodes the same way. The default is
   zstd's own (3). Writers may store a unit as `None` when compressing does not help.
5. **`zstd` is built without default features.** The `legacy` feature decodes pre-1.0 zstd frames, an old and
   unneeded parser; `zdict_builder` and `arrays` are not used.
6. **zstd contexts are cached per thread**, so decoding many 16 KiB blocks does not allocate a context each time.

## Consequences

- `persia-format` links zstd's C library (built from source by `zstd-sys`; needs a C compiler, no other tools).
  Miri cannot run C: tests that call zstd are `#[cfg_attr(miri, ignore = ...)]`; everything before the FFI call
  (codec ids, the bomb guard, `None`) still runs under Miri. The fuzz target `decompress` covers the FFI path,
  but zstd's C code is not coverage-instrumented, so libFuzzer only steers through our wrapper; mutated real
  frames (its odd mode) stand in. zstd's own decoder is fuzzed upstream (OSS-Fuzz).
- zstd's encoder output may change between zstd versions; only *decoding* is pinned by a golden frame. Writers
  must not rely on byte-identical compressed output across versions (checksums cover the stored bytes).
- The frame checksum flag of zstd is not used: the block CRC32C (SPEC §5.2) already covers the stored bytes.

## Alternatives considered

- **lz4 / snappy**: faster to decode, but a second dependency and a worse ratio for doc stores; zstd at low
  levels is close enough. A codec id leaves room to add one later.
- **Trusting the zstd frame header alone** (no separate `uncompressed_len`): frames may omit the content size,
  and the block header needs the length anyway for `None` blocks and for buffer planning.
- **Streaming decoder with an output limit**: handles frames without a content size, but accepts more shapes
  than needed and is slower for small blocks.
