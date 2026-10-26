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
