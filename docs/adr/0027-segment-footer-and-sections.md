# ADR-0027: Segment footer, section kinds and section encodings

- Status: proposed
- Date: 2026-10-26
- Related: SPEC §5.1, §5.2, §5.3 (§5.3.1–§5.3.6), §6.2.5, §15; ROADMAP 5.1, 5.2 (5.2.1–5.2.9)

## Context

SPEC §5 named a segment's sections and what each holds, but not their bytes. It also left open which sections are
block-framed (ROADMAP 5.2.9), and how Freqs, Skip, Positions, Norms, Columns, DocStore row offsets and the footer are
laid out. The segment writer (ROADMAP 5.2) needs all of that fixed. So will the reader (5.3), merges (6.x) and every
golden file from now on. Two SPEC statements did not survive contact: a footer of "4096 bytes max" cannot hold the
statistics and section table of a 1024-field schema, and the TermInfo record had no way to find a term's frequencies.

## Decision

1. **Footer** (SPEC §5.1). A fixed 88-byte part (format major/minor, `incompat_flags`/`rocompat_flags` as in the
   container header, segment id, schema hash with its `encoding_version` and field count, `doc_count`,
   `merge_generation`, `created_at`, two counts), then per-field statistics, the section table, `min_id`/`max_id` and a
   CRC32C. A trailing locator `footer_len u32` + `PSEG` finds it. The footer may be up to **1 MiB**. A reader still
   fetches the last 4 KiB first, which covers typical schemas, and fetches the rest only when `footer_len` asks for
   it. Statistics cover every `text`/`keyword` field, including zeros, so a reader indexes them by field without a
   search.
2. **Section kinds and order.** Codes 1–12 in the SPEC §5.3 order. Segment-level sections use field id `0xFFFF_FFFF`.
   Sections are laid out, and listed, in ascending `(field_id, kind)` and 8-aligned, so a field's sections sit
   together, which suits range reads. An unknown kind is corruption: adding a kind needs an `incompat_flags` bit.
3. **Framing** (ROADMAP 5.2.9). Everything is block-framed except the two FSTs (`TermDict`, `IdMap`). Those are read
   whole and used in place by `fst::Map`, so they are raw: payload, then a CRC32C trailer. Every byte stays
   checksummed.
4. **`IdMap` split.** The SPEC's "FST + array" becomes two kinds: `IdMap` (FST, id → docid) and `IdList` (record
   array, docid → id). Each direction then has its own framing and can be fetched alone.
5. **TermInfo** is a 56-byte record. It adds `freqs_off`, which the SPEC's field list lacked, and the term's
   `max_tf` and `min_norm`. MaxScore and block-max WAND need a per-term score bound before iterating; without it, a
   reader would scan every Skip entry of a long list (or decode a short one) for each query term, often from object
   storage. The bound pairs the largest tf with the shortest field, possibly from different documents, so it is valid
   for any BM25F parameters but loose; Lucene's (freq, norm) Pareto sets are tighter and could come later behind a
   flag. `skip_off` is 0 when
   `df < 128`, and `positions_off` is 0 without positions. Fixed width keeps lookup O(1) from the FST ordinal.
6. **Freqs** store `tf − 1` in blocks aligned with the postings blocks, without their own count. **Skip** has one
   40-byte entry per block, tail included, for every term with at least one full block. Entries hold all three
   offsets (postings, freqs, positions), so `advance` never decodes skipped blocks of any stream. Shorter lists are a
   single tail block and get no entries.
7. **Positions.** There is one run per postings block (bit-packed 128-groups plus a varint tail), with
   delta-in-document values. A **gap of 100** separates the values of a multi-valued field, Lucene's
   `position_increment_gap`.
8. **Norms** use Lucene's `SmallFloat.intToByte4` with its exact bounds: exact below 40, and rounding down by less
   than 12.5 %. A monotonic code makes `min_norm` a valid bound for block-max WAND. The **field length** is the token
   count: stacked tokens count too, which no built-in index analyzer produces today.
9. **Columns** start with a tag. A shared FOR column layout (count, block ends, blocks) gives O(1) access to any
   document's FOR block, and a shared bitmap layout uses roaring's portable format.
   - Numeric columns are presence plus keys. A missing document holds its block's minimum, so it costs no bits.
   - Bool columns are presence plus a `true` bitmap.
   - Keyword columns are dictionary + starts + ords, with the starts and ords offsets in the header so a cold remote
     read needs no chain of dependent fetches, so multi-valued keywords and facets work. This one encoding
     covers every cardinality in v1. The dictionary duplicates the `TermDict` keys, because an FST cannot map an
     ordinal back to its key cheaply.
10. **Record arrays.** `DocStore`, `IdList` and `BlobRefs` all use one layout: `u64` ends, then the records, zstd
    blocks. Rows may span blocks, as SPEC §5.2 already allows.
11. **Builder rules.** A segment has at least one document. Within one segment a duplicate id is rejected; upserts
    across segments are the write path's business (§8.3). A rejected document (invalid, duplicate, segment full, or an
    internal limit such as positions past `u32::MAX`) leaves the builder exactly as it was. Output is deterministic for
    the same input, meta, options **and zstd library version**: the compressed sections depend on zstd's output.

## Consequences

- The on-disk format depends on two crates' serializations: `fst` (map format 3) and roaring's portable format, the
  standard one shared by CRoaring and Java. The golden segment `tests/fixtures/segment/v1.0/basic.pseg` pins both.
  A dependency update that changed either would fail that test instead of silently changing the format. The golden
  test compares decoded content (block streams, raw sections, footer fields), not compressed bytes, and writes the
  fixture at a pinned zstd level, so a zstd update does not break a frozen fixture.
- About 24 bytes per document for the three record arrays' ends, before zstd. 56 bytes per term for TermInfo, before
  any compression; inlining tiny posting lists (pulsing) is left for later.
- The keyword dictionary costs a second copy of the distinct values.
- Readers (ROADMAP 5.3) must validate every rule SPEC §5.3 lists. The writer cannot violate them, and the
  differential test decodes every segment it writes with an independent, strict decoder.

## Alternatives considered

- **Keep a 4096-byte footer, with statistics in a section.** That adds a round trip to read a section before
  statistics are known, for every segment. A bounded but larger footer fetched speculatively costs nothing in the
  common case.
- **Block-frame the FSTs too.** `fst::Map` needs contiguous bytes, so a reader would decode every block into a copy
  anyway. Framing the FSTs buys per-block CRCs, which the trailer CRC already replaces.
- **Per-term allocation in the builder (`HashMap<String, Vec<Posting>>`).** That is simpler, but it allocates per
  term and per list growth. The flat posting log plus a counting sort by term at flush is O(n) and allocates per
  field, not per term.
- **No CRC trailer on the FSTs.** `fst` map format 3 ends in its own CRC32C, but the trailer makes every raw section
  self-checking the same way, whatever the payload, for 4 bytes.
- **Variable-length (varint) TermInfo.** It is smaller, but needs an index to stay O(1). It is worth revisiting with
  pulsing.
