# Benchmark baselines

Reference numbers for later comparison (for example, a SIMD decoder against the scalar baseline, ADR-0004).
They are single-machine snapshots, not targets (SPEC §16.2). Re-run them on the same machine before
comparing, and add a new dated row instead of editing an old one. Automated regression tracking is ROADMAP 1.11.2.

The comparative campaign — other engines, shared corpora, and what may be claimed — is specified in
[`BENCHMARKS.md`](../BENCHMARKS.md). That file is a methodology plan and has no results. The tables on
this page are internal microbenchmark snapshots. They are not product claims and they are not
comparisons against other engines.

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

## `persia-engine` segment merging

`cargo bench -p persia-engine --bench segment -- segment_merge`: the 2 000 documents of the segment building
benchmarks, written as 8 segments of 250, every 10th document of each deleted. `copy_8` is `merge` (ROADMAP 6.5.2:
every live posting, position, column value, row, id and blob ref copied into a builder); `copy_and_write_8` adds
`MergedSegment::write`. Live documents per second; compare with `segment/add` and `segment/finish`, which build the same
documents from scratch.

| Recorded | `segment_merge/copy_8` | `segment_merge/copy_and_write_8` | `segment/add`, same run |
|---|--:|--:|--:|
| 2026-10-28, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 6.5.2) | 44.8 K | 30.2 K | 70.0 K |

The copy is slower than analyzing the documents again. Taking parts out of the merge, one at a time, put about 26 ms
of the 40 ms in the reader (per term: a `Postings` cursor, then `next` and `freq` block by block), 8 ms in regrouping
the copied postings by document, and 4 ms in interning terms. A merge-specific reader that decodes whole lists is
ROADMAP 6.5.7.

## `persia-engine` WAL

`cargo bench -p persia-engine --bench wal`: 2 000 encoded documents of about 40 words. `append_buffered` writes them one
`Buffered` record at a time through `WalWriter` (chunks rotate at 64 KiB, about 8 chunks) and syncs once; `append_batch_fsync` is one
`append_batch` with `Fsync`; `replay` reopens the container and replays every chunk into a memtable (decoding each
document and indexing it). The log is a `SimFile`, so no time is spent in a real sync. Document bytes per second.

| Recorded | `append_buffered` | `append_batch_fsync` | `replay` |
|---|--:|--:|--:|
| 2026-10-27, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 6.2) | 356 MiB/s | 462 MiB/s | 66.1 MiB/s |

## `persia-engine` database write path

`cargo bench -p persia-engine --bench database`: 2 000 documents of about 40 words (the WAL bench's). `put_buffered`
puts them one at a time with `Buffered` durability through `Database::put` (stage in the memtable, log, publish);
`commit` flushes the 2 000 into one segment, appends its frame and the Manifest frame and writes the slot. The
container is a `SimFile`, so no time is spent in a real sync. Documents per second.

| Recorded | `put_buffered` | `commit` |
|---|--:|--:|
| 2026-10-28, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 6.3) | 269 K | 205 K |

Commits that tombstone older copies (ROADMAP 6.4.3): `commit_upserts` commits 2 000 documents that each replace one in
4 older segments (one sidecar written); `commit_inserts_<n>_segments` commits 2 000 new ids, each still looked up in
`n` older segments. Measured in one run with `commit`, so the four compare. Documents per second.

| Recorded | `commit` | `commit_upserts` | `commit_inserts_4_segments` | `commit_inserts_64_segments` |
|---|--:|--:|--:|--:|
| 2026-10-28, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 6.4, 6750fe8) | 201 K | 153 K | 174 K | 97.4 K |

## `persia-engine` query execution

`cargo bench -p persia-engine --bench search`: one segment of 50 000 documents of 5 to 34 words drawn from 200
(skewed toward low numbers, so `w3` is on about a sixth of the documents). Top 10 (exhaustive before ROADMAP 7.4.4, then
block-max WAND): `term` is `w3`; `and` is `w3 AND w40`; `or` is `w3 OR w40 OR w120`;
`phrase` is `"w0 w1"`. Time per query.

| Recorded | `term` | `and` | `or` | `phrase` |
|---|--:|--:|--:|--:|
| 2026-10-30, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 7.4.1–7.4.3 baseline) | 393 µs | 373 µs | 1.44 ms | 635 µs |
| 2026-10-30, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 7.4.4, block-max WAND by default) | 401 µs | 375 µs | 1.16 ms | 633 µs |
| 2026-10-30, same machine and session as the row above, the 7.4.3 baseline commit (aa803d0) | 455 µs | 369 µs | 1.42 ms | 617 µs |

`search_top10_pruning`: the same top 10 with each `Pruning` (ROADMAP 7.4.4); `or_rare` is `w3 OR w150 OR w190`, a
common word beside two rare ones. The corpus's blocks are much alike (every block holds short documents and
repeated words), so a term alone skips few blocks. Time per query.

| Recorded | query | `Exhaustive` | `MaxScore` | `BlockMaxWand` |
|---|---|--:|--:|--:|
| 2026-10-30, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 7.4.4) | `term` | 388 µs | 397 µs | 403 µs |
| | `or` | 1.36 ms | 902 µs | 1.17 ms |
| | `or_rare` | 780 µs | 270 µs | 432 µs |

Conjunctions and `should` beside `must` (ROADMAP 7.4.8): `and` is `w3 AND w40`, `and_common` `w0 AND w1`,
`must_should` `w3` with `should` `w40`, `w120`, and `must_should_rare` `w0` with `should` `w150`, `w190`. Before
7.4.8 every strategy walked these exhaustively, the three strategies of each query within 1.5 % of each other
(`and` 415 to 419 µs, `and_common` 480 to 487 µs, `must_should` 967 to 981 µs, `must_should_rare` 675 to 677 µs).
After, from one run on the same machine (2026-11-05, rustc 1.99.0, AMD Ryzen 5 3600, Linux), in which the exhaustive
walks ran 6 % (`and`) to 10 % (`must_should_rare`) slower than in the before run, so compare within a row. Those
walks do what they did before 7.4.8 (an in-tree A/B of `must_should` with the earlier `Optional` measured the same), so
the drift is taken to be the machine's:

| query | `Exhaustive` | `MaxScore` | `BlockMaxWand` |
|---|--:|--:|--:|
| `and` | 444 µs | 444 µs | 448 µs |
| `and_common` | 507 µs | 516 µs | 525 µs |
| `must_should` | 1.07 ms | 767 µs | 772 µs |
| `must_should_rare` | 746 µs | 303 µs | 313 µs |

This corpus's blocks are much alike, so block-max AND finds nothing to skip in `and` and `and_common` and costs
1 to 4 % for checking the bounds (a corpus whose block bounds differ skips most blocks:
`block_max_and_skips_blocks_that_cannot_beat_the_threshold` in `tests/pruning.rs`). A `must` beside `should`
gains most where its own bound cannot beat the threshold: then only documents holding a `should` clause are scored.

`minimum_should_match` (ROADMAP 8.1.6, L2 `drop_terms`): `or4` is `w3 OR w20 OR w60 OR w120`, and `or4_msm2` and
`or4_msm3` are the same four with at least 2 and at least 3 required. `or50` is 50 clauses, every fourth word (`w0`,
`w4`, … `w196`), and `or50_msm2` is `or50` with at least 2 required. One run (2026-11-09, rustc 1.99.0, AMD Ryzen 5
3600, Linux). A count of 2 costs block-max WAND what the plain disjunction does (`or4` against `or4_msm2`, `or50`
against `or50_msm2`). A count of 3 makes it cheaper (`or4_msm3`), because fewer documents can reach the pivot. A
count only drops documents, so the exhaustive walk gets cheaper as the count rises: it scores fewer documents and
skips from a document on too few clauses to the first one that could hold enough. An A/B against `main` in the same
session (`--save-baseline main` from a worktree), over the `term`, `and`, `or` and `or50` rows of `search_top10` and
the same rows plus `or_rare` of `search_top10_pruning`, moved every row by −3 % to +3.5 %, with mixed signs and `and`
(whose code did not change) moving as much: noise, not a regression of the plain disjunctions.

| query | `Exhaustive` | `MaxScore` | `BlockMaxWand` |
|---|--:|--:|--:|
| `or4` | 2.09 ms | 1.45 ms | 1.54 ms |
| `or4_msm2` | 1.58 ms | 1.45 ms | 1.54 ms |
| `or4_msm3` | 984 µs | 1.35 ms | 1.17 ms |
| `or50` | 24.3 ms | 41.5 ms | 23.9 ms |
| `or50_msm2` | 23.8 ms | 41.7 ms | 23.2 ms |

`search_suite` (ROADMAP 7.8.3): whole requests end to end over the same 50 000 documents in 4 segments (`body` with
positions), on rayon's global pool: search, merge, and the page's 10 documents read (each planned once, outside the timed loop). `term` is `w3`; `and` is the
user text `w3 w40` (SPEC §7.1.1); `or` is `w3 OR w40 OR w120`; `phrase` is the user text `"w0 w1"`; `filtered` is
`w3` with `price < 100`; `sorted` is `w3` by `price` ascending; `facets` is `w3` with a `tag` facet; `explain` is
`term` with every hit explained (SPEC §7.7), and `explain_filtered` is `filtered` explained, one pass per view
reusing the search's filter selections (ROADMAP 7.7.3). Command: `cargo bench -p persia-engine --bench search -- search_suite
--warm-up-time 1 --measurement-time 3`. Time per request.

| Recorded | `term` | `and` | `or` | `phrase` | `filtered` | `sorted` | `facets` | `explain` | `explain_filtered` |
|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|
| 2026-11-01, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 7.8.3) | 242 µs | 220 µs | 637 µs | 304 µs | 463 µs | 344 µs | 544 µs | 280 µs | 2.14 ms |
| 2026-11-02, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 7.7.3; only `explain_filtered`) | — | — | — | — | — | — | — | — | 566 µs |
| 2026-11-02, same machine and session as the row above, main before this change (only `explain_filtered`) | — | — | — | — | — | — | — | — | 1.42 ms |

`search_expansion_collection` (ROADMAP 7.4.14): `prefix` `w1` (111 terms, capped at 50) and `fuzzy` `w15` at distance
1, planned (`SearchPlan::with_terms`) over the same documents in 4 segments plus a memtable of 10 000 more, so every
term is listed by five views. Planning only. `search_top10_expansion` is the same queries planned and searched over
the single segment. Command: `cargo bench -p persia-engine --bench search -- "search_expansion_collection|search_top10_expansion"`.
Time per query.

| Recorded | collection `prefix` plan | collection `fuzzy` plan | one segment `prefix` | one segment `fuzzy` |
|---|--:|--:|--:|--:|
| 2026-11-02, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 7.4.14) | 826 µs | 618 µs | 25.8 ms | 21.7 ms |
| 2026-11-02, same machine and session, the commit before 7.4.14 | 3.59 ms | 2.31 ms | 25.1 ms | 21.0 ms |
| 2026-11-05, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 7.4.15, its commit; A/B with the row below, alternating, two runs each) | 708–724 µs | 611–616 µs | 25.2–25.6 ms | 21.2–21.6 ms |
| 2026-11-05, same machine and session, main before 7.4.15 | 840–848 µs | 619–623 µs | 26.7–26.8 ms | 22.3–22.7 ms |

The 7.4.14 one-segment queries were about 3 % slower: a term that loses the cap is no longer cached, so each run read its
`df` from the segment again. Since 7.4.15 the views' walks are merged and each term's `df` is the sum of their own counts,
with no lookup. The one-segment queries are 4–6 % faster in the A/B above, more than the lookups saved explain (the
collection plan, with five views per term, saves about 125 µs): part of it is likely code layout, not fewer lookups.

`search_top10_filtered_sweep` (ROADMAP 7.4.16): the sweep the per-candidate check shares are fitted on (SPEC §7.3).
Top 10 over the single segment of 50 000 documents, a `must` clause from rare to frequent (one word, or an OR of two
to four words) under `price < 100` (10 %) alone and beside the `keyword` `tag = t3` (10 %), the column filter selected
as a bitmap (share 0, what a `bool` with a `keyword` filter did before 7.4.16) or checked per candidate (share 1).
The cost is the `must` clause's estimate, the per mille of the view the threshold compares (an OR's sums its words'
`df`s). Command: `cargo bench -p persia-engine --bench search -- search_top10_filtered_sweep --warm-up-time 1
--measurement-time 2`. 2026-11-05, rustc 1.99.0, AMD Ryzen 5 3600, Linux; one run (a first run of the single words
and the ORs from 181 ‰ agreed within 10 %). Time per query; **bold** is the faster.

| `must` | cost ‰ | `range` bitmap | `range` check | `keyword` + `range` bitmap | `keyword` + `range` check |
|---|--:|--:|--:|--:|--:|
| `w199` | 0 | 517 µs | **54.3 µs** | 621 µs | **108 µs** |
| `w170` | 28 | 578 µs | **187 µs** | 645 µs | **250 µs** |
| `w140` | 56 | 635 µs | **266 µs** | 657 µs | **312 µs** |
| `w150 OR w190` | 56 | 717 µs | **370 µs** | 699 µs | **447 µs** |
| `w110` | 83 | 669 µs | **339 µs** | 665 µs | **345 µs** |
| `w80` | 107 | 699 µs | **418 µs** | 668 µs | **372 µs** |
| `w80 OR w190` | 116 | 700 µs | **353 µs** | 726 µs | **632 µs** |
| `w50` | 134 | 740 µs | **492 µs** | 682 µs | **405 µs** |
| `w25` | 155 | 755 µs | **552 µs** | 686 µs | **420 µs** |
| `w10` | 168 | 761 µs | **587 µs** | 681 µs | **433 µs** |
| `w0` | 173 | 775 µs | **604 µs** | 693 µs | **441 µs** |
| `w0 OR w190` | 181 | 721 µs | **414 µs** | **750 µs** | 761 µs |
| `w50 OR w150` | 182 | 935 µs | **664 µs** | **755 µs** | 794 µs |
| `w25 OR w120` | 229 | 1.03 ms | **877 µs** | **768 µs** | 882 µs |
| `w0 OR w120` | 247 | 1.03 ms | **791 µs** | **781 µs** | 917 µs |
| `w0 OR w60` | 298 | 1.17 ms | **1.05 ms** | **798 µs** | 1.00 ms |
| `w0 OR w20` | 330 | 1.21 ms | **1.03 ms** | **825 µs** | 1.01 ms |
| `w0 OR w1` | 345 | 1.24 ms | **1.16 ms** | **846 µs** | 1.01 ms |
| `w0 OR w1 OR w2` | 516 | **1.78 ms** | 2.02 ms | **946 µs** | 1.46 ms |
| `w0 OR … OR w3` | 687 | **2.21 ms** | 3.08 ms | **1.04 ms** | 1.92 ms |

With column filters alone a check wins up to about 35 % of the view, single words and ORs alike, and loses from
50 %: `CHECK_SHARE` is 3 (a third; 7.4.13's first guess was a quarter). Beside a `keyword` filter its bitmap is
selected either way, but a check leaves the `must` clause walking its matches among the `keyword`'s 10 % rather than
among the 1 % of both filters. A check wins for every single word (up to 173 ‰, the most common word) and for ORs
up to 116 ‰, and loses for ORs from 181 ‰ (by 1–5 % up to a fifth, more beyond). No single word costs more than
173 ‰ here, so where a single word stops winning is not measured. `CHECK_SHARE_BESIDE_POSTINGS` is 5 (a fifth): a
sixth (167 ‰) would give up the single words at 168–173 ‰ (a check is 36 % faster there) to spare ORs at 167–200 ‰
a few percent. The deciding cost is the `must` clause's, not the candidates the `keyword` bitmap leaves: those are at
most 10 % in every row, and the check still loses from 181 ‰.

`search_top10_filtered/term/keyword_range_10pct` (`w3` under `tag = t3` and `price < 100`), same machine and session:
412 µs at the fitted share; 677 µs with both filters as one bitmap (`…/bitmap`, as before 7.4.16). The other
`search_top10_filtered` queries keep their path (`term/range_10pct` 550 → 537 µs, `term/range_50pct` 723 → 724 µs,
`rare_term/range_10pct` 227 → 221 µs against main in the same session).

`search_keyword_columns` (ROADMAP 7.5.5): 40 000 documents in 4 segments plus 5 000 in the memtable, each with its
own `sku` (a `keyword` field, `facet: true`: every segment's dictionary holds 10 000 values), 10 hits of `w3` per
request, a fresh memtable view and views each time, every view on the calling thread. Command: `cargo bench -p
persia-engine --bench keyword_columns -- --warm-up-time 2 --measurement-time 5`. 2026-11-05, rustc 1.99.0, AMD
Ryzen 5 3600, Linux; main run from a worktree with the same bench file, alternating, twice (both runs within 2 %).

| Recorded | `sort_sku` | `score_facet_sku` | `sort_sku_facet_sku` | `sort_price` |
|---|--:|--:|--:|--:|
| ROADMAP 7.5.5 | 1.94 ms | 5.57 ms | 5.84 ms | 854 µs |
| main before 7.4.15–7.5.5 | 2.48 ms | 5.94 ms | 7.67 ms | 996 µs |

A segment's dictionary is decoded and checked once while the block cache keeps it (`keyword_column` on one of these
segments: 115 µs per call before, 12 µs after, in a one-off timing loop of 100 calls, not a checked-in benchmark),
and a memtable view copies a field's keyword column and column keys
once, so a sort and a facet over the same field share the copy. On rayon's pool (`views.search`) the memtable view
is the slowest task, so the wall time moves less: `sort_sku_facet_sku` 7.28 → 5.29 ms, `score_facet_sku` 5.29 →
4.60 ms, `sort_sku` slower, not explained: 1.92 and 1.94 → 2.01 and 1.99 ms in the two alternating
runs (3–5 %), and 1.84–1.93 → 1.98–2.05 ms over every run (up to 11 % between extremes). Its memtable view, copied
once either way, sets the wall time; on the calling thread the same request is 21 % faster. The one-segment
`search_request/sort_price` and `score_facet` (a `tag` of 10 values, an empty memtable) have no dictionary worth
keeping: −0.6 % to +4 % against main over three runs, within this machine's noise.

`search_keyword_columns` after ROADMAP 7.5.7 (the memtable's views share their column copies), the same requests
on the calling thread and on rayon's global pool (`search_keyword_columns_pool`, added to the bench in 7.5.7).
2026-11-06, same machine and command; main (3c171e3, with the same bench file, from a worktree) and the branch
alternating, twice (`main` and `7.5.7` columns are each run's median; the two runs agree within 3 %, the pool's facet
rows within 10 %):

| Request | main, thread | 7.5.7, thread | main, pool | 7.5.7, pool |
|---|--:|--:|--:|--:|
| `sort_sku` | 2.18 / 2.13 ms | 0.999 / 1.02 ms | 1.80 / 1.83 ms | 0.564 / 0.560 ms |
| `score_facet_sku` | 5.68 / 5.73 ms | 4.84 / 4.77 ms | 4.57 / 4.83 ms | 3.92 / 4.30 ms |
| `sort_sku_facet_sku` | 6.29 / 6.14 ms | 5.08 / 5.07 ms | 4.93 / 5.11 ms | 4.46 / 4.48 ms |
| `sort_price` | 1.04 / 1.01 ms | 0.820 / 0.808 ms | 485 / 483 µs | 424 / 434 µs |

Each iteration takes a new memtable view of the same 5 000 documents, so after the first one every view reads the
copy shared by the first one. In the probe described below (the memtable view's task, medians over two runs of it at
7.5.5 and two on this branch), the memtable view of `sort_sku` takes 96–97 µs on the calling thread on this branch
against 910 µs at 7.5.5, and 138–139 µs on the pool against 1 320–1 490 µs, so on the pool the four segment views
now set the wall time. The facet rows move less: counting every match in the segments dominates them.

**The 7.5.5 pool regression** (`sort_sku` 3–5 % slower than before 7.5.5 on the pool, 21 % faster on the calling
thread). Found with a throwaway probe (not checked in): the bench's setup, then 3 000 `sort_sku` requests timed in
three stages (views and plan, `search_with`, dropping the page, views and memtable view), with the start and end of
each view's task recorded around `Execution::run`. Medians over two runs, before 7.5.5 (7aa3a1d) and at 7.5.5
(00ab8b7):

| | before 7.5.5, thread | 7.5.5, thread | before 7.5.5, pool | 7.5.5, pool |
|---|--:|--:|--:|--:|
| memtable view's task | 1 000 µs | 910 µs | 1 440–1 560 µs | 1 320–1 490 µs |
| `search_with` | 2 410 µs | 1 850 µs | 1 580–1 680 µs | 1 510–1 600 µs |
| drop after the search | 1 µs | 89 µs | 2 µs | 230–310 µs |
| whole request | 2 420 µs | 1 950 µs | 1 590–1 700 µs | 1 760–1 910 µs |

The search itself was not slower at 7.5.5, on the calling thread or on the pool. The drop was. Before 7.5.5 the
memtable view's keyword column (5 000 boxed values, the ords and the starts) was built by the sort and freed with it,
inside the view's task on the worker, in parallel with the segments. At 7.5.5 the view keeps it until the view drops, after the search, on the
calling thread, alone on the critical path. On the pool the copy was allocated on a rayon worker, and freeing it on
another thread cost about 3 times as much as on the thread that allocated it: 230–310 µs against 89 µs. That fits
glibc's allocator, which keeps only a few freed chunks per size in the freeing thread's cache and returns the rest to
the allocating thread's arena under its lock (not measured apart). On the calling
thread the 89 µs is less than what the segments' cached dictionaries save (4 × 100 µs); on the pool those savings are
off the critical path, so only the drop showed. At 7.5.7 the shared copy outlives the request (it is freed when a copy
of more documents replaces it, or with the memtable), and the drop is 0–2 µs on both.

**Pool against calling thread.** Before 7.5.7, the memtable view's task alone took 1.4–1.6 times longer on the pool
than on the calling thread (p10 950–1 050 µs, p90 2.0–2.3 ms, against 880–1 060 µs on the calling thread), and each
segment view about 2.2 to 2.5 times longer (700 µs against 330 µs before 7.5.5, 550 µs against 220 µs at 7.5.5).
On the pool the five views run at once on a 6-core, 12-thread CPU: they share cores (SMT), memory bandwidth and the
allocator, and a worker may run on the other core complex than the one whose cache holds the memtable the writer
filled. The probe did not separate these. Their sum is why the pool's wall time is well above the slowest view's
time on the calling thread, and why it varies (up to 11 % between runs). A request that no longer copies the memtable
is less exposed to it: with the probe at 7.5.7, `sort_sku` on the pool is 525 µs (the bench above: 560–564 µs), the
memtable view's task 138–139 µs.

`search_keyword_columns_write` (ROADMAP 7.5.7) upserts one memtable document before each `sort_sku` request
(untimed setup, `BatchSize::PerIteration`), so every request's memtable view has one more document than the last.
It copies its columns again and replaces the shared copy, the worst case for the cache. The replaced copy is freed
after the memtable's locks are released, so a writer does not wait for it. Same session, main and the branch
alternating, twice:

| | main | 7.5.7 |
|---|--:|--:|
| `sort_sku`, calling thread | 2.59 / 2.60 ms | 2.50 / 2.51 ms |
| `sort_sku_pool` | 3.80 / 3.98 ms | 3.79 / 3.86 ms |

No slower than main: the request copies once either way. The pool is slower than the calling thread here, unlike in
the read-only rows. A likely cause is that the pool's workers go idle during each untimed upsert and must be woken
for the next request; this was not measured apart. The memtable grows by one docid per iteration (a few thousand
over a run), and criterion picks the iteration count from the elapsed time, so a faster build's later requests copy a
few more documents: compare builds at equal iteration counts, which criterion prints.

`search_request/score_then_newest` (ROADMAP 7.4.11): the one-segment `search_request` collection, 10 hits of `w3`
sorted by score, then `at` descending (a tie-break by recency). Term proximity (ROADMAP 7.3.5, ADR-0036): the AND
rows, whose body has positions; `and_no_proximity` is `and` without the bonus. Command: `cargo bench -p
persia-engine --bench search -- 'search_request/score|search_top10/and|search_top10_pruning/(and|and_common)/|search_suite/and'`.
2026-11-07, rustc 1.99.0, AMD Ryzen 5 3600, Linux. Before 7.4.11 is 5862e6f with the same bench file, before 7.3.5 is
c2488c9, each from a worktree with its own target directory; the three alternating, twice (first run, second run).

| Row | before | after |
|---|--:|--:|
| `search_request/score` | 456, 448 µs | 451, 459 µs |
| `search_request/score_then_newest` | 720, 728 µs | 510, 518 µs |
| `search_request/score_facet` | 870, 881 µs | 849, 876 µs |
| `search_top10/and` | 439, 437 µs | 528, 542 µs |
| `search_top10/and_no_proximity` (removed: `and` without the bonus, today's `search_top10/and`) | | 448, 456 µs |
| `search_top10_pruning/and/exhaustive` | 430, 433 µs | 748, 786 µs |
| `search_top10_pruning/and/max_score` | 437, 432 µs | 516, 539 µs |
| `search_top10_pruning/and/block_max_wand` | 444, 445 µs | 531, 552 µs |
| `search_top10_pruning/and_common/block_max_wand` | 510, 520 µs | 634, 656 µs |
| `search_suite/and` | 271, 238 µs | 389, 351 µs |

These rows were measured with the bonus on by default, as 7.3.5 was first written. It is now off by default
(ADR-0036 item 5), so `search_top10/and`, the `search_top10_pruning` rows and `search_suite/and` run without it, as in
the before column, and `search_top10/and_proximity` (which replaced `and_no_proximity`) is `and` with it.

7.4.11: before, a sort with a key after the score visited every match and read each one's `at` key. Now it prunes
like the score alone, strictly below the worst kept hit, and a match scoring below that is rejected before any
column is read. Pruning alone (a first version, without that early reject) measured 713 µs: one term over this corpus
has few blocks to skip, so most of the gain is the column reads saved. Where blocks do lose (`tests/search_request.rs`,
`a_sort_led_by_the_score_prunes_with_further_keys`), the pruned search reads less than a third of what the exhaustive
one with a facet reads.

7.3.5: the bonus reads both words' positions for each document a conjunction scores, through the words' own postings
(`Matcher::field_positions`): +78 % exhaustively. Pruned, a candidate whose clauses plus the bonus's bound cannot beat
the threshold is skipped before its positions are read: `search_top10/and` +22 %, `search_suite/and` +45 % (its
before runs vary by 13 %). Two first versions measured more: their own postings per pair term (decoding every docid
and frequency block twice) gave +106 % on `search_top10/and` and ×2 on `search_suite/and`; with the early skip, +39 %
and ×2. `must_should` (one `must`) has no pair and is unchanged.

`search_relaxed/*` (ROADMAP 8.1.2): the 4-segment collection on the calling thread, `w3 w40` as user text, 10 hits,
planned and searched each iteration. `plain` is `SearchPlan::with_terms` then `search_with`, `l0` is
`search_relaxed` with the collection's levels (L0 has `min_results` hits, so no later level is built), and `off` is
`relax: false`. Command: `cargo bench -p persia-engine --bench search -- search_relaxed`. 2026-11-08, rustc 1.99.0,
AMD Ryzen 5 3600, Linux.

| Recorded | `plain` | `l0` | `off` |
|---|--:|--:|--:|
| ROADMAP 8.1.2 | 448 µs | 449 µs | 447 µs |

The same branch against 9691454 (main, worktree with its own target directory) on `search_top10/*`, whose code it
does not touch, and on `search_request` and `search_suite`, whose `search_with` it splits into stages: at the default
16 codegen units `search_request/score` measured 440–448 µs on main and 487–509 µs on the branch. The 8.1.1 commit
alone, which adds the `relax` module and changes no search code, measured 502 µs. Built with one codegen unit
(`CARGO_PROFILE_BENCH_CODEGEN_UNITS=1`), main and the branch agree: `search_request/score` 466, 454 µs vs 449, 467 µs,
`search_top10/term` 463, 448 µs vs 452, 450 µs, and `search_request/score_facet` 847, 854 µs vs 839, 889 µs. The
default build's gap is codegen-unit layout from new code in the crate, not the cascade's work.

The relaxed levels (ROADMAP 8.2.1–8.2.3), same collection and thread. `l1` is `w3 nosuch` relaxed to L1 `synonyms`;
`l2` is `w3 w40 w7 w120 zz` relaxed to L2's first step with a hit, and `l2_every_step` the same with the largest
`min_results` (10 000), so it reaches every rung, down to `drop_terms:1`, whose matches it need not all collect; `l3` is `w3 w` with L3 `prefix` alone, its one-letter last word
expanded over every term (ROADMAP 8.1.20). Command: `cargo bench -p persia-engine --bench search -- search_relaxed`.
2026-11-12, rustc 1.99.0, AMD Ryzen 5 3600, Linux. Main is c8d6496, built in a worktree with its own target directory.

| Recorded | build | `l1` | `l2` | `l2_every_step` | `l3` |
|---|---|--:|--:|--:|--:|
| main (c8d6496) | default | 879 µs | 1.38–1.41 ms | 12.84–13.28 ms | — |
| ROADMAP 8.2.2.4–8.2.3 | default | 902 µs | 1.45–1.48 ms | 11.52–11.83 ms | 23.0 ms |
| main (c8d6496) | 1 codegen unit | 883 µs | 1.48 ms | 12.67 ms | — |
| ROADMAP 8.2.2.4–8.2.3 | 1 codegen unit | 926 µs | 1.43 ms | 11.47 ms | 22.2 ms |

`l2_every_step` gains about 10% with ROADMAP 8.2.2.4: its last steps exclude L0 and one L2 step instead of every
earlier one, so they plan fewer excluded queries. `l2` is about 5% slower in the default build and about 3% faster
with one codegen unit; it stayed slower in the default build with `DropTerms::nested` edited to return `false` (a
local change, not a bench option), so that move is layout, not work. `l1` is 3–5% slower in both builds, unexplained:
its step is the same query, and its code only reads the root through `Compiled::root` now.

## `persia-engine` relevance

`cargo test -p persia-engine --test relevance` (ROADMAP 7.8.2, 7.8.4): 18 user-text queries with graded judgments over
54 store items and how-to articles (`crates/persia-engine/tests/relevance/`), BM25F over `title` (boost 2) and `body`,
`en` analyzer. Per-query values are in `crates/persia-engine/tests/relevance/baseline.txt`, the baseline of the regression gate: every value
and the mean must stay within `0.0001` (one unit in the fourth decimal, the rounding a platform's `ln`/`powf` may flip)
of it. A drop beyond that fails as a regression, and a rise fails too, so an improvement lands only with the baseline
replaced on purpose by the table the failure prints, the PR naming the queries that moved. The tolerance is below
half the smallest change one judged document makes moving by one rank, so the gate sees every such move in the top 10
(two documents of one grade trading places change nothing). It runs in `cargo test --workspace` (0.05 s; CI's `test` job, Linux and macOS). A row
is added here when the mean moves.

| Recorded | mean nDCG@10 | lowest |
|---|--:|---|
| 2026-11-01 (ROADMAP 7.8.2) | 0.8824 | 0.7173 `garden`, 0.7364 `bread`, 0.7453 `"cast iron" pan`, 0.7648 `tent` |

The proximity bonus (ROADMAP 7.3.5, ADR-0036), turned on, left every row unchanged: under AND each multi-word query matches 1
to 3 documents, and only 5 of those matches (4 queries) have their words apart, none reordered. ROADMAP 15.1.6
adds judged queries the bonus can decide. The gate runs with the default plan, which leaves the bonus out.

## `persia-blob` hashing and chunking

`cargo bench -p persia-blob --bench blob`: incompressible bytes. `sha256` is `blob_hash/sha256_one_shot_4MiB` (the
blob id of 4 MiB, ADR-0002); `chunk_all` is `blob_chunk/chunk_all_32MiB_4MiB`, a 32 MiB blob cut into default 4 MiB
chunks (SHA-256, CRC32C, and the copy of each chunk out of the reader, SPEC §9.2). One core, Zen 2 with SHA
extensions. Bytes per second.

| Recorded | `sha256` | `chunk_all` |
|---|--:|--:|
| 2026-11-07, rustc 1.99.0, AMD Ryzen 5 3600, Linux (ROADMAP 9.1.1, 9.1.2, 63be88a) | 1.87 GiB/s | 1.31 GiB/s |
