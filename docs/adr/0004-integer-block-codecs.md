# ADR-0004: Integer block codecs (delta, bit-packing, postings blocks, FOR)

- Status: accepted
- Date: 2026-10-10
- Related: SPEC §5.2.1, §5.3; ROADMAP 1.2.1–1.2.4, 5.2.2, 5.2.5, 5.3.3

## Context

Postings, freqs and numeric columns hold most of a segment's bytes, and decoding them is the query hot path.
SPEC §5.3 already said "blocks of 128, delta-coded, bit-packed; tail block varint" and "numerics = FOR/bit-pack",
but did not give the byte layout. These encodings are part of the first segment format version. No version
has been released yet, so there is nothing to bump from. After this, changing them means a format version bump.

## Decision

1. **Gaps minus one.** Docid gaps are stored as `v_i - v_(i-1) - 1`; the first value of a list is taken
   relative to `-1`. Strictly increasing order then holds by construction: the decoder only has to check for
   `u32` overflow, not for zero gaps. A dense run costs width 0.
2. **Vertical 4-lane bit-packing for 128-value blocks** (the SIMD-BP128 layout; Lemire & Boytsov, 2015).
   The scalar code that ships now is no harder than for a horizontal layout. A later SSE2/NEON/AVX2 decoder
   reads the same bytes with one shift/mask per 128-bit row, so SIMD is purely a speed-up, with no format change.
3. **Minimal postings block header: `last_docid u32`, `width u8`.**
   - `last_docid` lets an iterator skip a block without unpacking it, and it is checked twice.
     - When the header is read, it must be at least `prev + 128` (127 for a list's first block). This
       covers skipped blocks too, so a list stays strictly increasing whatever mix of skip and decode is used.
       Without it, a skipped block could seed the next block's `prev` with a value that goes backwards.
     - When the block is decoded, it must equal the last decoded docid.
   - Block-max data (`max_tf`, `min_norm`) lives only in the Skip section (SPEC §5.3). It is the single
     source for block-max WAND, and it is meaningful only as a pair: `max_tf` alone would assume the
     segment-wide shortest field.
   - The tail block (`n mod 128` docids) stores its own `count`. The section reader checks it against
     `df mod 128`.
4. **FOR blocks of exactly 1024 values (the last may be shorter), horizontally bit-packed.**
   - Columns are read by docid, so O(1) random access matters more than SIMD batch decode: docid `d` is
     value `d mod 1024` of block `d div 1024`.
   - A block stores `min u64`, `count u32` (1..=1024, checked by the reader), and `width u8` (0..=64),
     followed by a plain LSB-first bit stream.
   - The fixed size gives `count` a bound the reader enforces itself. Otherwise a 13-byte width-0 block
     could claim 2^32 values.
   - The header costs about 0.1 bit per value.
5. **Order-preserving `u64` keys** for `i64` (flip the sign bit) and `f64` (the `total_cmp` mapping). Range
   filters and sorting then compare keys as unsigned integers, and FOR applies to every numeric type. Whether
   `-0.0`/NaN are canonicalized on write is decided with the document model (ROADMAP 3.2.6).
6. **Canonical encoding is a writer obligation.**
   - Writers must use minimal widths, the true FOR minimum, and zero padding bits, so output is
     deterministic (CLAUDE.md "Format discipline").
   - Readers reject non-zero padding bits but accept non-minimal widths and a FOR `min` below the true
     minimum: these still decode to the right values.
   - Checking minimality in the hot path is deferred until a benchmark shows its cost (for bit-packing it is
     an OR per value during unpack). An offline `verify` (ROADMAP 5.4) may check it instead.

## Consequences

- One scalar codec today. SIMD can be added later without a format change; the baseline is the
  `persia-format` benches (ROADMAP 1.2.2.5).
- A full postings block carries a 5-byte header, about 0.31 bits per docid.
- The Skip section (ROADMAP 5.2.2) must carry `max_tf` and `min_norm` for every block, including the tail.
- Blocks are byte-aligned and unaligned within their section. Decoders work on bytes (little-endian loads),
  never on transmuted pointers, so alignment does not matter for correctness.
- Golden byte vectors for every codec live in the unit tests of `persia-format` (`bitpack`, `postings`,
  `for_codec`), and fuzz targets cover each decoder.

## Alternatives considered

- **Plain deltas (`v_i - v_(i-1)`).** These cost one more bit for dense runs, and the decoder has to reject
  zero gaps.
- **Horizontal layout for postings.** It is simpler to describe, but SIMD would later need a format change or a
  repacking step.
- **`max_tf` (and `min_norm`) inside the postings block.** This duplicates the Skip section. `max_tf` alone is
  too loose a block-max bound for BM25F (it must assume the segment-wide shortest field).
- **Unbounded FOR `count`.** This leaves docid→block lookup undefined and moves the count bound onto every
  caller.
- **PFOR / exceptions (Lucene `PForUtil`).** These compress better when a block has a few outliers, but they
  mean more code and more branches. They can be added later behind a format version bump if benchmarks justify it.
- **Varint everywhere.** Simple, but several times slower to decode, with no random access for columns.
- **Freqs inside the postings block.** SPEC §5.3 keeps Freqs a separate section, so that docid-only scans
  (filters, `match_all` counts) do not read them.
