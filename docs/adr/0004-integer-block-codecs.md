# ADR-0004: Integer block codecs (delta, bit-packing, postings blocks, FOR)

- Status: proposed
- Date: 2026-10-09
- Related: SPEC §5.2.1, §5.3; ROADMAP 1.2.1–1.2.4, 5.2.2, 5.2.5, 5.3.3

## Context

Postings, freqs and numeric columns hold most of a segment's bytes and decoding them is the query hot path.
SPEC §5.3 already said "blocks of 128, delta-coded, bit-packed; tail block varint" and "numerics = FOR/bit-pack",
but not the byte layout. These encodings are part of the first segment format version (none has been
released yet, so there is no older version to bump from). After that, changing them means a format version bump.

## Decision

1. **Gaps minus one.** Docid gaps are stored as `v_i - v_(i-1) - 1` (the first value of a list relative to `-1`).
   Strictly increasing order then holds by construction: the decoder only has to check for `u32` overflow,
   not for zero gaps. A dense run costs width 0.
2. **Vertical 4-lane bit-packing for 128-value blocks** (the SIMD-BP128 layout: Lemire & Boytsov, 2015).
   The scalar code that ships now is no harder than for a horizontal layout. A later SSE2/NEON/AVX2 decoder
   reads the same bytes with one shift/mask per 128-bit row, so SIMD is a pure speed-up, with no format change.
3. **Self-describing postings blocks.** A full block starts with `last_docid u32`, `max_tf u32` and `width u8`.
   `last_docid` lets an iterator skip a block without unpacking it, and doubles as a consistency check
   (the decoded last docid must match it). `max_tf` is the input for block-max scoring. The Skip section
   (SPEC §5.3) may repeat these so a reader can skip blocks without reading them at all. The tail block
   (`n mod 128` docids) stores its own `count`, so a mismatch with the term's `df` is detectable.
4. **Horizontal bit-packing for FOR columns.** Columns are read by docid, so O(1) random access matters more
   than SIMD batch decode. A FOR block stores `min u64`, `count u32` and `width u8` (0..=64), then a plain
   LSB-first bit stream. The unused bits of the last byte must be zero, so there is a single valid encoding.
5. **Order-preserving `u64` keys** for `i64` (flip the sign bit) and `f64` (the `total_cmp` mapping), so range
   filters and sorting compare keys as unsigned integers, and FOR applies to every numeric type.
6. **Minimal widths** are required of writers (deterministic output, CLAUDE.md "Format discipline") but are
   not checked by readers: checking would cost an extra pass over every block in the hot path. A
   non-minimal width still decodes to the same values.

## Consequences

- One scalar codec today; SIMD (ROADMAP 1.2.2.5 is the baseline benchmark) can be added without a format change.
- A full postings block carries a 9-byte header (about 0.56 bits per docid).
- Blocks are byte-aligned and unaligned within their section. Decoders work on bytes (little-endian loads),
  never on transmuted pointers, so alignment does not matter for correctness.
- Golden byte vectors for every codec live in the unit tests of `persia-format` (`bitpack`, `postings`,
  `for_codec`), and fuzz targets cover each decoder.

## Alternatives considered

- **Plain deltas (`v_i - v_(i-1)`).** One more bit for dense runs, and the decoder has to reject zero gaps.
- **Horizontal layout for postings.** Simpler to describe, but SIMD would later need a format change or a
  repacking step.
- **PFOR / exceptions (Lucene `PForUtil`).** Better compression when a block has a few outliers, but more code
  and branches. It can be added later behind a format version bump if benchmarks justify it.
- **Varint everywhere.** Simple, but several times slower to decode, and it gives no random access for columns.
- **Freqs inside the postings block.** SPEC §5.3 keeps Freqs a separate section so that docid-only scans
  (filters, `match_all` counts) do not read them.
