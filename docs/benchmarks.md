# Benchmark baselines

Reference numbers for later comparison (for example, a SIMD decoder against the scalar baseline, ADR-0004).
They are single-machine snapshots, not targets (SPEC §16.2). Re-run them on the same machine before
comparing, and add a new dated row instead of editing an old one. Automated regression tracking is ROADMAP 1.11.2.

## `persia-format` integer codecs (scalar)

- Command: `cargo bench -p persia-format --bench bitpack --bench blocks -- --warm-up-time 1 --measurement-time 2`
- Recorded: 2026-10-10, rustc 1.99.0, AMD Ryzen 5 3600, Linux

Throughput is in values per second (G = 10^9, M = 10^6).

| Benchmark | Parameter | Throughput |
|---|---|--:|
| `bitpack/unpack` (128 × u32) | width 1 / 4 / 8 / 13 / 20 / 32 | 1.74 / 1.52 / 1.41 / 1.24 / 1.06 / 0.86 G |
| `bitpack/pack` (128 × u32) | width 1 / 4 / 8 / 13 / 20 / 32 | 0.85 / 0.48 / 0.37 / 0.27 / 0.21 / 0.20 G |
| `postings_block/decode` (unpack + delta + checks) | mean docid step 2 / 64 / 4096 | 778 / 693 / 680 M |
| `for_block/iter` (1024 values, includes block validation) | width 7 / 20 / 64 | 341 / 343 / 352 M |
| `for_block/get_random` (1024 random lookups) | width 7 / 20 / 64 | 242 / 262 / 262 M |
| `for_block/decode_into` (1024 values, includes block validation; 2026-10-27, ROADMAP 5.3.16) | width 7 / 20 / 63 / 64 | 755 / 739 / 722 M / 3.49 G |

The `iter` and `get_random` rows were recorded when the keys held only 48 random bits, so their width-64 case
encoded at width 48. Since 2026-10-27 every case encodes at its labelled width, as the `decode_into` row was measured.

## `persia-analysis` analyzers

- Command: `cargo bench -p persia-analysis --bench analyzers -- --warm-up-time 1 --measurement-time 3`
- Input: 256 field values of 12 words each, drawn by a fixed LCG from the Snowball vocabulary of the analyzer's
  language (English for the others), a third of them stopwords, with some capitals and punctuation.
- Throughput is in tokens per second, counting what the analyzer's tokenizer emits (`keyword`: one per value);
  `fold` counts tokens through `FoldingFilter::preserving(['ñ'])` alone.

| Recorded | Commit | `standard` | `whitespace` | `keyword` | `en` | `es` | `pt` |
|---|---|--:|--:|--:|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 4.4.5 baseline | 12.4 M | 21.5 M | 29.0 M | 2.47 M | 1.52 M | 1.56 M |

| Recorded | Commit | `fold/ascii` | `fold/spanish` | `fold/non_ascii_unfoldable` |
|---|---|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 4.4.5 baseline | 120 M | 20.8 M | 6.28 M |
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 4.4.9 pre-scan | 134 M | 18.6 M | 20.0 M |

| Recorded | Commit | `standard` | `whitespace` | `keyword` | `en` | `es` | `pt` |
|---|---|--:|--:|--:|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 4.4.4 `TokenStream` | 12.9 M | 29.5 M | 27.1 M | 2.46 M | 1.64 M | 1.58 M |

After the 4.4 batch B review (fold pre-scan checks characters before NFC; `fr`, `de`, `it` added in 4.4.10):

| Recorded | `standard` | `whitespace` | `keyword` (values/s) | `en` | `es` | `pt` | `fr` | `de` | `it` |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 14.3 M | 28.7 M | 27.7 M | 2.45 M | 1.56 M | 1.59 M | 1.23 M | 2.01 M | 1.41 M |

| Recorded | `fold/ascii` | `fold/spanish` | `fold/non_ascii_unfoldable` |
|---|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 160 M | 21.0 M | 20.6 M |

After 4.4.12 (stemmers and folding work in `Scratch` buffers):

| Recorded | `standard` | `whitespace` | `keyword` (values/s) | `en` | `es` | `pt` | `fr` | `de` | `it` |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 14.7 M | 32.7 M | 27.6 M | 3.05 M | 1.70 M | 1.66 M | 1.38 M | 2.40 M | 1.50 M |

| Recorded | `fold/ascii` | `fold/spanish` | `fold/non_ascii_unfoldable` |
|---|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 154 M | 22.8 M | 20.5 M |

## `persia-engine` segment building

`cargo bench -p persia-engine --bench segment`: 2 000 synthetic documents (an 8-word `title` with positions, a 120-word
`body`, a `keyword`, an `f64`; Zipf-like vocabulary of 5 000 words, fixed seed). `add` is `SegmentBuilder::add` for all
of them (validation, CBOR row, analysis, accumulation); `finish` is `SegmentWriter::finish` on the built segment.
Documents per second:

| Recorded | `segment/add` | `segment/finish` |
|---|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 68.7 K | 98.9 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; sections streamed into the segment, lists encoded twice (ROADMAP 5.1.6; the parent commit measured 100.5 K) | | 82.7 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; before ROADMAP 5.1.6 (its parent on the branch squashed into b185ab1), measured with the next two rows | | 100.3 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; main at b185ab1 (ROADMAP 5.1.6 with its review follow-ups) | | 96.2 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; lists measured from bit widths, blocks drained without allocating (ROADMAP 5.1.7) | | 99.0 K |

The 18 % that ROADMAP 5.1.6 cost in its own run (82.7 K) did not reproduce. The three rows above are single runs, one
after another: 5.1.6 with its review follow-ups was 4 % slower than its parent, and ROADMAP 5.1.7 1.3 % slower. A
repeat that alternated the two builds measured main (b185ab1) at 92.5 K and 96.9 K and ROADMAP 5.1.7 at 97.8 K and
100.2 K: 5.1.7 is faster than main in both pairs, by 3–6 %, but repeats of the same build differ by 2.5–5 %, more
than its 1.3 % gap to the parent, which these runs cannot resolve.

## `persia-engine` segment reading

`cargo bench -p persia-engine --bench segment -- segment_read`: the segment the building benchmarks write (default
`WriteOptions`). `open` is `Segment::open` (footer parse and section checks); `term_get` is
`TermIndex::get` of each of the 4 909 `body` terms; `postings_freq` iterates every `body` posting with `freq`;
`advance` steps every 64th docid on the `body` terms with `df >= 200`; `positions` iterates every `title` posting with
its positions; `document` fetches every stored document by docid. Operations per second (opens, lookups, postings,
`advance` calls, postings, documents):

| Recorded | `open` | `term_get` | `postings_freq` | `advance` | `positions` | `document` |
|---|--:|--:|--:|--:|--:|--:|
| 2026-10-26, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 2.38 K | 5.34 M | 6.69 M | 4.82 M | 453 K | 434 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; parent commit of ROADMAP 5.3.11 (609e833), measured with the next row | 2.03 K | 5.37 M | 7.27 M | 4.97 M | 485 K | 488 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; per-segment block cache (ROADMAP 5.3.11), positions per document (5.3.13) | 2.02 K | 6.83 M | 46.6 M | 21.7 M | 6.56 M | 1.26 M |

Compare rows measured together: runs on different days differ by 10–20 % on this machine. `open` did not change with
ROADMAP 5.3.11 (2.03 K and 2.02 K in the same session); the 2.38 K of 2026-10-26 is the earlier session, not a
regression. The segment in these benchmarks is small, so after the first pass almost every block is cached: the
gains are those of re-reading, and a first read of a block costs what it did before.

`segment_read/norms_get` reads every document's `body` norm with `Norms::get`, `norms_cursor` through `Norms::cursor`
(ROADMAP 5.3.14, 5.3.16), in norms per second. Since 5.3.14 norms are read through the block cache: `get` costs one
cache lookup, the cursor one per framed block.

| Recorded | `norms_get` | `norms_cursor` |
|---|--:|--:|
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 9.84 M | 1.20 G |

## `persia-engine` segment opening and record lookups

`cargo bench -p persia-engine --bench segment -- "segment_open|segment_records"`. `segment_open/large_dict` is
`Segment::open` of a segment with one `keyword` field holding a distinct value per document (200 000 documents): a
`TermDict` of 200 000 terms and an `IdMap` of as many ids, both checked node by node (`fst_check`) and walked once.
`segment_records/id_of_1000` is 1 000 `Segment::id_of` lookups spread over a 50 000-document segment of 1 KiB blocks
(ROADMAP 5.3.10: a lookup's cost must not grow with the section's block count). Terms per second, lookups per second:

| Recorded | `segment_open/large_dict` | `segment_records/id_of_1000` |
|---|--:|--:|
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux | 2.26 M | 407 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; parent commit of ROADMAP 5.3.11 (609e833), measured with the next row | (not run) | 399 K |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; per-segment block cache (ROADMAP 5.3.11) | (not run) | 2.30 M |

## `persia-engine` column scans

`cargo bench -p persia-engine --bench segment -- segment_columns`: a 50 000-document segment (default `WriteOptions`)
whose `f64` keys span the key range (64-bit FOR blocks, many crossing a framed block). `numeric_key_scan` reads every
document's key with `NumericColumn::key`; `keyword_ords_scan` every document's ords with `KeywordColumn::ords`.
`numeric_key_cursor` and `keyword_ords_cursor` read the same through `NumericColumn::cursor` and
`KeywordColumn::cursor` (ROADMAP 5.3.16), which keep their decoded FOR blocks to themselves and unpack a block with
`ForBlock::decode_into`.
Values per second. The lazy readers of ROADMAP 5.3.12 trade speed for memory: before them a column was a decoded
`Vec` of 8 bytes per document (first row, measured on the parent commit); now one block per FOR stream (one for a numeric column, two for a keyword column) stays decoded.

| Recorded | `numeric_key_scan` | `keyword_ords_scan` | `numeric_key_cursor` | `keyword_ords_cursor` |
|---|--:|--:|--:|--:|
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; eager columns (before ROADMAP 5.3.12) | 491 M | 651 M | | |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; lazy columns (ROADMAP 5.3.12) | 30.0 M | 11.4 M | | |
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux; cursors (ROADMAP 5.3.16) | 31.9 M | 10.8 M | 214 M | 59.5 M |

## `persia-engine` memtable

`cargo bench -p persia-engine --bench segment -- memtable`: the 2 000 documents of the segment building benchmarks.
`put` is `Memtable::put` for all of them (validation, the full CBOR encoding, analysis, accumulation, the per-term
chains, under the write lock), the counterpart of `segment/add`; `postings_freq` reads every `body` term's postings
through a `MemtableView` (copied out under the read lock) with frequencies, as `segment_read/postings_freq` does.
Documents and postings per second.

| Recorded | `memtable/put` | `memtable/postings_freq` | `segment/add`, same run |
|---|--:|--:|--:|
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 6.1) | 78.9 K | 45.4 M | 69.6 K |

## `persia-engine` WAL

`cargo bench -p persia-engine --bench wal`: 2 000 encoded documents of about 40 words. `append_buffered` writes them one
`Buffered` record at a time through `WalWriter` (chunks rotate at 1 MiB) and syncs once; `append_batch_fsync` is one
`append_batch` with `Fsync`; `replay` reopens the container and replays every chunk into a memtable (decoding each
document and indexing it). The log is a `SimFile`, so no time is spent in a real sync. Document bytes per second.

| Recorded | `append_buffered` | `append_batch_fsync` | `replay` |
|---|--:|--:|--:|
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 6.2) | 356 MiB/s | 462 MiB/s | 66.1 MiB/s |
