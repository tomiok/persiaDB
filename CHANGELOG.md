# Changelog

All notable, user-visible changes to Persia DB are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). On-disk format versions are tracked separately
(SPEC §15) and are called out under **Format**, a project-specific section added to the standard ones.

## [Unreleased]

### Added
- Workspace skeleton (stub crates).
- Apache-2.0 license.
- Project website under `website/` (Astro Starlight), for GitHub Pages at `https://tomiok.github.io/persiaDB/`.

### Fixed
- Website: the article column is centered between the left nav and the narrow "On this page" rail, at Starlight's 45rem reading width.

### Format
- Integer codecs for segment sections (SPEC §4.1, §5.2.1; ADR-0004): zigzag signed varints, delta coding of
  sorted docids, 128-value bit-packed blocks (SIMD-BP128 layout), postings full and tail blocks, and
  frame-of-reference blocks with order-preserving keys for numeric columns.
- Checksum and hash primitives (SPEC §4.1): CRC32C (one-shot and streaming), xxh3-64/128 (seed 0), and
  `Checksummed<T>` for fixed-size structures whose CRC32C field covers the rest of the region (SPEC §4.2, §4.3).
- Block codecs (SPEC §5.2; ADR-0005): codec byte `0 = None`, `1 = Zstd` (one zstd frame with declared content
  size), with a decompression-bomb guard on every decode.
- Block framing of segment sections (SPEC §5.2; ADR-0006): 16-byte block header (CRC32C, codec, lengths), fixed-size
  blocks over a byte stream, and a CRC-protected block table that records the block size (the bomb limit) and
  each block's end; corrupt blocks are reported with their index. The section table has no `codec` field.
  A block table can be opened from the section's tail alone and blocks decoded from separately fetched bytes
  (object storage). Decoding errors now separate corruption from caller errors and from running out of memory
  (an unallocatable decompression buffer is no longer reported as corrupt data).
- Container header and superblock (SPEC §4.2, §4.3; ADR-0007): `Header` (magic, format version, feature flags,
  database id, page size) and double-buffered `Superblock` slots with CRC32C, and selection of the active slot. An
  unknown `format_major` or `incompat_flags` bit is a new `Unsupported` error class rather than corruption; unknown
  `rocompat_flags` bits open the container read-only; reserved bytes must be zero; two valid slots with the same
  `seq` but different contents are corruption. Each slot is a full 4096-byte page (A at 4096, B at 8192, first frame
  at 12288) so one bad sector cannot lose both (ADR-0008); `page_size` is an advisory alignment hint; a writer never
  lowers `format_minor`.
- Frames (SPEC §4.4; ADR-0009): encode and validate the checksummed records after the superblock, with 8-byte
  padding, a kind registry whose unknown kinds are skipped, and optional zstd payloads that carry their uncompressed
  length and decode under a caller-supplied bomb limit. A frame scanner stops at the first invalid frame and reports a
  clean end, a torn tail (only zeros or nothing follow), corruption (non-zero bytes follow) or a frame with unknown
  flags, which is `Unsupported` and never truncated.

<!--
Sections per release, in this order, only when non-empty:
### Added / ### Changed / ### Deprecated / ### Removed / ### Fixed / ### Security / ### Format
-->

[Unreleased]: https://github.com/tomiok/persiaDB/commits/main
