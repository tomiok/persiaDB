# ADR-0006: Block framing of segment sections

- Status: accepted
- Date: 2026-10-10
- Related: SPEC §4.1, §5.1, §5.2, §5.3, §17; ADR-0005; ROADMAP 1.5.1–1.5.6, 5.2.9

## Context

SPEC §5.2 said large sections are split into fixed-size blocks, each with a `{crc32c, codec, uncompressed_len}`
header, but gave no byte layout, no way to find block *i* without scanning, and no place for the block size that
readers need as their decompression-bomb limit (ADR-0005). It also left open what the section table's `codec`
field means next to per-block codecs, and how a DocStore row larger than one block is stored. No format version
has been released, so this defines the layout rather than changing it.

## Decision

1. **16-byte block header**: `crc32c u32, codec u8, 3 reserved, uncompressed_len u32, stored_len u32`. The CRC
   comes first and covers the rest of the header and the payload, so it is verified before any field is used;
   a bit flip anywhere is a checksum mismatch. `stored_len` makes a block self-delimiting and is cross-checked
   against the block table. `u32` lengths suffice because blocks are at most 1 MiB. 16 bytes per 16 KiB block
   is about 0.1%.
2. **A section is a byte stream cut at fixed uncompressed boundaries.** Block `i` holds stream bytes
   `[i·block_size, (i+1)·block_size)`, all full except the last. A stream position maps to a block by one
   division, with no per-block uncompressed offsets stored, and the reader knows every block's exact
   `uncompressed_len` in advance, so a lying header is caught before decoding.
3. **A block table ends the section**: one `block_end u64` per block (relative to the section start), then a
   fixed 24-byte trailer `total_len u64, block_size u32, block_count u32, reserved u32, crc32c u32`. A writer can
   stream blocks without knowing their count; a reader finds the trailer from the section's `len`, then the table
   (two range requests, or one that over-fetches the tail), and then fetches any block on its own (ROADMAP
   1.5.6). The table has its own CRC, so a corrupt table is reported as such, not as a block error.
4. **`block_size` lives in the block table**, capped at 1 MiB. The section therefore carries its own bomb limit,
   and the footer's section table needs no extra field. The cap bounds what any reader allocates per block,
   whatever the table claims. Each block is also at most `16 + block_size` bytes long (writers fall back to `None`
   when zstd does not shrink a block), so a CRC-valid but hostile table cannot make one block fetch gigabytes.
5. **The section table has no `codec` field.** Every block names its codec, and whether a section is
   block-framed is a property of its `section_kind`. A section-level codec would only be a second, possibly
   contradicting, source of truth.
6. **Records may span blocks.** DocStore rows (and any record in a block-framed section) are addressed by stream
   position; a reader decodes the blocks that overlap the record. A large document then costs no special case,
   and blocks keep a fixed size for caching.
7. **Block errors carry the block index** (`Error::CorruptBlock`) besides the absolute offset, so tools such as
   `persia verify` can name the damaged block. Caller errors are not corruption: a request beyond the stream
   is `Error::OutOfBounds`, a section tail shorter than its table is `Error::NeedMoreBytes` (fetch `needed` and
   retry), and a block slice of the wrong length is `Error::InvalidInputLen`.

## Consequences

- Readers validate the whole table on open (O(blocks)); decoding a block then needs no further table checks.
- A block is at least 16 bytes and the section has 24 bytes of fixed overhead, even when empty.
- Writers buffer at most one block of input, plus one compressed copy and the encoded bytes the caller has not
  yet drained (`BlockWriter::take_output`), so a large section can be streamed to storage.
- Blocks and the block table are byte-aligned, an explicit exception to SPEC §4.1: `u64` fields are read with
  `from_le_bytes`, never through a zero-copy `&[u64]` view. Only the section start is aligned.
- A block CRC covers the block's own bytes, not its index or section: two equal-length blocks swapped inside
  one section pass their CRCs. Segments are immutable and written in one pass, so this needs a misdirected
  write inside a segment; detecting it is left to segment-level integrity (footer CRC, ROADMAP 5.2.7) rather than
  a per-block field.

## Alternatives considered

- **Variable-size blocks cut at record boundaries**: no record spans blocks, but blocks are no longer found by
  division, a per-block uncompressed offset must be stored, and one huge record makes one huge block (a worse
  bomb limit).
- **Block table at the section start**: readable first, but the writer must buffer the whole section or seek back.
- **`u32` block offsets**: smaller table, but a DocStore section may exceed 4 GiB.
- **Block size in the footer's section table**: needs the footer to read a section, and couples the section
  format to the footer's.
