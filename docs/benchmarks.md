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
