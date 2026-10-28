# Benchmark methodology

> **Plan only. No results exist.** This file specifies how Persia DB will be measured. It does not report a measurement, and it does not make a performance claim.
>
> No performance claim may be made until two things are true: every MVP milestone (M0–M15 in [`ROADMAP.md`](./ROADMAP.md)) is complete, and a run of **this** plan has been published under the rules in [What may be claimed](#12-what-may-be-claimed). Building the harness during M15 is preparation. It is not that publication.
>
> **How to read a digit in this file.** A number here is one of: a target quoted from [SPEC §16.2](./SPEC.md#16-quality-requirements), the regression threshold quoted from [SPEC §16.3](./SPEC.md#16-quality-requirements), a product default quoted from the SPEC, a methodology parameter (a rule for a future run), or the placeholder `TBD`. None of those is a measured result. If a figure is not in one of those classes, this file is wrong and should be fixed before anyone cites it.

## Contents

1. [Status and when this plan may run](#1-status-and-when-this-plan-may-run)
2. [Where this file sits](#2-where-this-file-sits)
3. [Principles](#3-principles)
   - [Equivalent work](#31-equivalent-work)
   - [Correctness before the clock](#32-correctness-before-the-clock)
   - [Reproducibility](#33-reproducibility)
   - [Publish the losses](#34-publish-the-losses)
   - [No cherry-picking](#35-no-cherry-picking)
   - [Version pinning](#36-version-pinning)
   - [Vendor-recommended configuration](#37-vendor-recommended-configuration)
   - [Cold cache and warm cache](#38-cold-cache-and-warm-cache)
   - [Durability parity](#39-durability-parity)
   - [Statistical rigor](#310-statistical-rigor)
4. [Comparison categories](#4-comparison-categories)
5. [Competitors](#5-competitors)
6. [Datasets](#6-datasets)
7. [Workloads](#7-workloads)
8. [Relevance](#8-relevance)
9. [Environment](#9-environment)
10. [Harness](#10-harness)
11. [Reporting](#11-reporting)
12. [What may be claimed](#12-what-may-be-claimed)
13. [Performance work in M15.2](#13-performance-work-in-m152)
14. [Decisions left to the harness](#14-decisions-left-to-the-harness)
- [Appendix A. Result record](#appendix-a-result-record)
- [Appendix B. Checklist before a claim](#appendix-b-checklist-before-a-claim)
- [Appendix C. Equivalence checklist](#appendix-c-equivalence-checklist)

## 1. Status and when this plan may run

Persia DB is an embedded full-text search database written in Rust. Search, ranking, the server, object-storage backends, sharding, and the network SDKs are specified and not all implemented. A number taken against today's tree would describe a partial library, then be read as a product result.

Execution is gated:

| Step | When | What it produces |
|---|---|---|
| This document | Now | The methodology. No timings. |
| Harness construction ([ROADMAP 15.1](./ROADMAP.md)) | During M15. The milestone depends on M1–M14 and may start once M7 has landed. | Tools, dataset manifests, adapters. Still no claim. |
| Internal microbenchmarks | Already, and they continue | Snapshots in [`docs/benchmarks.md`](./docs/benchmarks.md). Codec, analyzer, segment, memtable, and WAL benches. Not a search-product comparison. |
| Publication campaign | Only after M0–M15 are complete (v1.0) | The first run that this file allows anyone to cite. |

Until that campaign is published, the honest public statement is: Persia DB does not publish search benchmarks. The product-shape comparison on the website ([comparison guide](./website/src/content/docs/guide/comparison.md)) stays a comparison of design, not of speed.

[SPEC §16.2](./SPEC.md#16-quality-requirements) already lists targets and labels them as targets to be validated, not promises. They are repeated in [Appendix A](#appendix-a-result-record) only so a future run has the scenario names in one place. Meeting or missing a target is a result that does not exist yet.

## 2. Where this file sits

This file is the master methodology for the end-to-end and comparative campaign. Other documents keep their jobs:

| Document | Role |
|---|---|
| [`BENCHMARKS.md`](./BENCHMARKS.md) (this file) | What will be measured, how, against whom, and what may later be claimed. |
| [`docs/benchmarks.md`](./docs/benchmarks.md) | Dated single-machine snapshots of in-repo Criterion benches. Add a new row; do not rewrite an old one. Not targets ([SPEC §16.2](./SPEC.md#16-quality-requirements)). Not comparisons with other engines. |
| [SPEC §16.2](./SPEC.md#16-quality-requirements) | Latency, throughput, open, and recovery **targets**. |
| [SPEC §16.3](./SPEC.md#16-quality-requirements) | Requires a reproducible harness, pinned configs, a regression gate, and methodology on every comparison. This file is that methodology. |
| [ROADMAP 15.1](./ROADMAP.md) | Builds the harness: datasets (15.1.1), workloads (15.1.2), embedded and server runner with HdrHistogram (15.1.3), methodology plus optional adapters (15.1.4), regression gate (15.1.5). This file is the methodology half of 15.1.4, written ahead of the adapters. It does not close the leaf. |
| [ROADMAP 15.2](./ROADMAP.md) | Performance work driven by profiles (SIMD unpack, scoring allocations, term-dict layout, cache defaults). Profiles may be taken while the harness is built. A profile is not a publication. See [Performance work in M15.2](#13-performance-work-in-m152). |
| [ROADMAP 1.11.2](./ROADMAP.md) | Weekly Criterion run, already implemented (`.github/workflows/bench.yml`). Stores artifacts. Does not gate. |
| [ROADMAP 7.8.2](./ROADMAP.md) and [7.8.3](./ROADMAP.md) | Relevance corpus with nDCG@10, and Criterion benches for term, boolean, phrase, filtered, sorted, and facet queries. Those benches feed `docs/benchmarks.md`. They are not the campaign. |
| [ROADMAP 8.4](./ROADMAP.md) | Relaxation quality: synthetic typos, zero-result rate, precision of the first relaxed hit. The campaign reports those beside latency. |
| [ROADMAP 10.4.3](./ROADMAP.md) | `persia bench`, a small local microbenchmark entry point. It does not run this campaign. |
| [ROADMAP 12.4.7](./ROADMAP.md) | Cold and warm query latency on MinIO. The object-storage workload below is the methodology for that leaf. |
| [ADR-0001](./docs/adr/0001-own-engine-and-allowed-dependencies.md) | The engine is ours. Tantivy, SQLite, and the other systems in this plan are **external programs** under test. They are not dependencies. See [Harness](#10-harness). |

If this file and the SPEC disagree, the SPEC wins and this file is a bug. If this file and `docs/benchmarks.md` seem to disagree, they are about different things: a plan here, snapshots there.

## 3. Principles

### 3.1 Equivalent work

A public comparison of terminal emulators once showed a large throughput advantage for an implementation that was discarding the input unparsed. The clock was real. The work was not the work the benchmark name implied (parse the stream and retain it). The comparison could not be used for anything.

That failure mode is the constraint on every row in this plan. A row is valid only when every engine in it does the same job on the same input. Faster because a step was skipped is a broken row, including when Persia is the one that skipped it.

Equivalent work means all of the following, for every engine in the row:

- The same source documents, produced by one shared preprocessing step, checksummed before any engine sees them.
- The same query strings, filters, sort, and result window.
- Scoring on when the workload says ranked search. A match-everything scan is a different workload.
- Stored fields fetched when the workload says fetch. An engine that cannot store the original field drops out of that row; the row is not redefined so that nobody fetches.
- The same durability class ([Durability parity](#39-durability-parity)).
- The same cache condition ([Cold cache and warm cache](#38-cold-cache-and-warm-cache)).
- Typo tolerance either on for every participant or off for every participant. Mixing Persia at exact match with a server whose typo tolerance is still on is a different amount of work.
- Positions indexed when the workload contains phrase queries. Persia stores positions only when the schema asks (SPEC §6). The workload turns them on, and turns the equivalent on everywhere else.

[Appendix C](#appendix-c-equivalence-checklist) is the list a reviewer ticks before a run is eligible to publish.

Work that v1 does not do is omitted from the campaign for every engine, with the SPEC section named. It is not left blank on the Persia column. Examples: text extraction and OCR (SPEC §1.2; clients supply text), vector search (SPEC §1.2), more than one writer on a shard (SPEC §8.4), highlighting (SPEC §7.7, post-MVP, ROADMAP M17).

### 3.2 Correctness before the clock

A timing run starts only after a correctness pass for that engine and that workload has passed. A failed pass is published as a failure. It does not contribute a latency.

The pass has four layers. Each layer's outcome is a field in the result record.

1. **Persia against its reference model.** Hit sets match, and scores match within the epsilon the engine's differential tests already use ([SPEC §7.2](./SPEC.md#7-search), [SPEC §16.1](./SPEC.md#16-quality-requirements), ROADMAP 7.8.1). The run record cites that epsilon. This plan does not invent a second tolerance. Sharded search with `global_stats: true` matches single-node results (ROADMAP 13.3.6).
2. **The work happened.** Indexed document count equals the source count. A fixed sample of document ids round-trips the stored source text. A fixed sample of planted terms, inserted by preprocessing and listed in the dataset manifest, is found by an exact query. This is the check that catches a system which discarded or skipped input and still returned quickly.
3. **Result-set equality, where it is meaningful.** Two engines with different analyzers and different ranking functions will not return the same order. Equality of hit sets is required only for a workload that configures both sides to the same analysis and a boolean (non-scored, or constant-score) query. Where equality is not required, the record says why.
4. **Recall against judgments, where judgments exist.** On a dataset with qrels, compute recall@k of the returned set against the judgments before accepting the timing. Relevance metrics themselves are [Relevance](#8-relevance). A system that returns an empty list quickly fails this layer.

Zero-result relaxation (M8) adds the quality metrics from ROADMAP 8.4.2 on the same run as the latency: zero-result rate before relaxation, zero-result rate after, and precision of the first relaxed hit. A latency figure for a relaxed query is not published without those.

### 3.3 Reproducibility

A published run can be repeated from one command ([One command](#106-one-command)) on a machine of the recorded class. The command checks pins and refuses to run when a checksum differs.

The result record contains the git SHA, the toolchain in [`rust-toolchain.toml`](./rust-toolchain.toml), every competitor version and checksum, dataset checksums, the workload manifest checksum, the configuration-file checksums, the seed for any generator, and the environment block in [Environment](#9-environment).

Trials of engines alternate (engine A, engine B, engine A, engine B, …) so a thermal or load drift does not sit entirely inside one engine's sample. The wall-clock start of each trial is stored.

### 3.4 Publish the losses

Every workload in the pre-registered manifest is in the published report, including workloads where Persia is slower or less relevant. The summary table is ordered by workload id. A loss is the same type size and the same section as a win.

Dropping a workload after seeing the number is a failed publication. A workload may be dropped only by editing the manifest **before** the first trial, with the reason written in the manifest (not implemented in v1, license, no equivalent feature on a participant the row requires).

### 3.5 No cherry-picking

The manifest is committed before the first trial. Its git SHA is the run's registration. Changing the manifest, the machine, the version, or the trial count after that starts a new run id. The previous run stays in the changelog.

One publication machine is named in the manifest. Publishing the best of several machines is not a run. Repeating a trial is allowed when the log shows an operational failure (crash, disk full, harness bug). The failed attempt stays in the raw archive. Repeating a trial because the figure was unwelcome is not allowed.

Other projects' benchmark blogs, including Elastic Rally's published numbers, are not imported as either side of a comparison. Rally tracks may be used as **datasets** when the license allows ([Datasets](#6-datasets)). The timings in this campaign are the ones this harness measured.

### 3.6 Version pinning

| Pin | Rule |
|---|---|
| Persia | Git SHA. After v1.0, a claim names a tag as well as the SHA the tag points at. |
| Toolchain | The channel in `rust-toolchain.toml`, and the `rustc` version the run actually used. |
| Competitor | Exact release or commit. The download URL and checksum are in the manifest. A floating tag such as `latest` is not a pin. |
| Dataset | Upstream archive checksum and the checksum of the canonical JSONL the adapters read. Dump date or version string as the publisher gives it. |
| Container image | Digest, when the category is a container run. A mutable tag is not a pin. |

A claim that says "current" without a SHA is not a claim this file allows.

### 3.7 Vendor-recommended configuration

Each engine is given its own recommended configuration. The headline comparison never pairs Persia-tuned with a competitor left on an untuned default, or the reverse.

Procedure, finished before any trial of that run:

1. Pin the version.
2. Read that version's own operations or benchmarking guide. Record the URL and the date it was read.
3. Ask the project, in a public issue or discussion, which settings they recommend for the workload text in the manifest. The ask states the durability class, the result window, that ranking is on, and whether typo tolerance is on.
4. The wait for a reply is chosen in the manifest before the run (`TBD` until a campaign is scheduled). "No reply by that date" is a recorded outcome.
5. Apply a recommendation that preserves the work in [Appendix C](#appendix-c-equivalence-checklist).
6. A recommendation that changes the work (turn off durability, turn off scoring, drop fields, shrink the result window, skip parsing) is stored in the run appendix and may be executed only as an extra row labeled `vendor-note-not-equivalent`. That row is never the only row for that workload.

Two matrices are published, and they are not crossed:

- **Defaults.** Persia uses the production-safe defaults (SPEC §14). Every other engine uses that version's defaults, with the durability class aligned as in [Durability parity](#39-durability-parity).
- **Recommended.** Each side uses the configuration gathered above, including a tuned Persia only when the tuning was written down before the run.

Product defaults the harness must set explicitly and record, so a later default change cannot silently move a number:

| Knob | Default this plan records | Source |
|---|---|---|
| Durability of an acknowledged write | `Fsync` | SPEC §8.2.1, [ADR-0028](./docs/adr/0028-wal-chunks-and-replay.md) |
| BM25F `k1` | 1.2 | SPEC §6.2 |
| BM25F `b` | 0.75 | SPEC §6.2 |
| Text field `boost` | 1.0 | SPEC §6.2 |
| `relax.min_results` | 1 | SPEC §7.6 |
| `relax.budget_ms` | 50 | SPEC §7.6. This budget is part of the work of a relaxation row. |
| Expansion cap (synonym, prefix, fuzzy) | 50 | SPEC §7.6 |
| Memtable flush | 64 MiB, or 10 s idle | SPEC §8.2 |
| WAL chunk rotation | 1 MiB target, 64 MiB maximum | SPEC §8.2.1 |
| Compaction trigger | tiered policy in SPEC §8.5 | SPEC §8.5 |

These are specification defaults, not observed speeds.

### 3.8 Cold cache and warm cache

Every latency workload reports two conditions. They are different rows. A warm Persia number is never placed next to a cold competitor number.

| Condition | Definition |
|---|---|
| Cold | New process. Application caches empty (Persia block cache starts empty because the process is new; competitor caches flushed the way that engine documents). OS page cache dropped, and the run record says whether the drop succeeded. First pass over the query set, reported separately from later passes. |
| Warm | After the warmup in [Statistical rigor](#310-statistical-rigor), caches left as the process filled them. |

On object storage the block cache (SPEC §10.6) dominates. A warm row without a hit ratio is not comparable. The run stores the hit and miss counters (ROADMAP 10.2.5, 11.4.7, 12.4). ROADMAP 12.4.7 is the MinIO cold-versus-warm pair; it follows this split.

Warmup samples are stored and are excluded from the headline percentiles.

### 3.9 Durability parity

The headline write path is the one where an acknowledged write survives a power loss.

For Persia that level is `Fsync`: the WAL chunk is written and synced before the call returns (SPEC §8.2.1). On Linux the container uses `fdatasync` where the SPEC says so (SPEC §4.5.2). `Buffered` (lost on process crash) and `Async` (survives process death, not machine failure) are opt-ins for bulk load. They may be published as their own labeled matrix. They are not the headline.

Each competitor adapter records the knob that matches that class and the value that was set, read from the pinned version's documentation at pin time. Examples of the knobs the adapter must identify, not values this plan freezes: SQLite's synchronous mode, Lucene's translog durability on Elasticsearch, OpenSearch, and Solr, Tantivy's commit sync, and the flush settings Meilisearch and Typesense document. The adapter author copies the version's name for the knob into the config file. This plan does not guess a default that may have changed.

A RAM disk, a `tmpfs`, or a bench that skips the sync is not a durability measurement. The in-repo WAL Criterion bench runs on an in-memory [`SimFile`](./crates/persia-storage/src/container/sim.rs) and spends no time in a real sync ([`docs/benchmarks.md`](./docs/benchmarks.md)). It may be published only as a CPU or format microbenchmark, in `docs/benchmarks.md`, under that label.

### 3.10 Statistical rigor

Methodology parameters for a future run. They are not measurements.

| Parameter | Rule |
|---|---|
| Trial, query latency and mixed load | One process. Warmup, then the query set. This measures steady state and excludes process start. Process start is the open workload. |
| Trial, index, open, recovery | A new process per trial. Cold trials also drop the OS cache. |
| Query-latency trial count | At least 30 trials after warmup. |
| Lifecycle trial count (index, open, recovery, compaction) | At least 5 trials. A campaign that cannot finish 5 does not publish that workload. |
| Headline percentiles | p50, p95, p99, p999, plus the maximum. Mean is recorded and is not the headline. |
| Histogram | HdrHistogram, 3 significant figures (ROADMAP 15.1.3; `hdrhistogram` is already a dev-dependency). |
| Interval | 95% bootstrap percentile interval of the across-trial distribution of each headline percentile. 10000 resamples. The resample unit is the trial, not a single query pooled across trials. |
| Overlapping intervals | The report says that no difference was detected at this sample size. It does not name a winner. |
| Outliers | Kept. A note may point at a logged event (compaction, a neighbor process). The sample is not deleted. |
| Variance | The report includes the interval above and the trial-level spread (interquartile range of the per-trial percentile). A single trial is never a publication. |
| Regression gate | A slowdown greater than 5% against a stored baseline fails the microbenchmark gate (SPEC §16.3, ROADMAP 15.1.5). That percentage is a threshold, not a slowdown that has been observed. |

Shared CI runners are too noisy for the 5% gate. The weekly workflow says so (`.github/workflows/bench.yml`, ROADMAP 1.11.2). Publication numbers come from the machine in [Environment](#9-environment), not from a GitHub-hosted runner.

## 4. Comparison categories

Network overhead, process boundaries, and durability settings change the work. Categories are not interchangeable. A table has one category in its caption.

### 4.1 Embedded against embedded

In-process libraries, same host, no HTTP and no proxy.

| Engine | Role in this category |
|---|---|
| Persia `Db` (ROADMAP M10, SPEC §8, §14) | The library under test. `Db::open` on a local file. |
| [Tantivy](https://github.com/quickwit-oss/tantivy) | Closest peer: a Rust library, segments on disk, caller owns serving. |
| [SQLite FTS5](https://sqlite.org/fts5.html) | In-process SQL full-text module in the SQLite file. |

Tantivy is not swapped for Quickwit or any other server that embeds it. Those are different products and would need their own row and an amendment of this file. SQLite appears only in this category.

### 4.2 Server against server

A separate process and a client over the network, on the same machine, over loopback, unless the workload is the sharded one in [Sharded readers](#79-sharded-readers). Loopback removes the wide-area network and keeps the protocol, the serialization, and the process boundary. That is the work of "using the server."

| Engine | Role in this category |
|---|---|
| Persia server (ROADMAP M11, SPEC §12) | gRPC is the primary transport. HTTP/JSON is a separate labeled client, not a silent substitute. |
| [Meilisearch](https://www.meilisearch.com/docs) | HTTP server. Typo tolerance defaults on; the row says whether it was left on. |
| [Typesense](https://typesense.org/docs/) | HTTP server. Same typo-tolerance rule as Meilisearch. |
| [Elasticsearch](https://www.elastic.co/docs) | One pinned version. Not averaged with OpenSearch. |
| [OpenSearch](https://docs.opensearch.org/latest/) | Its own pin, its own vendor ask. |
| Solr | Its own pin, its own vendor ask. Lucene-family servers are not collapsed into one column. |

The client is the engine's own official client when it has one, pinned. For Persia, the Rust and Go SDKs are measured in [SDK overhead](#710-sdk-overhead), and the server-category headline uses one of them, named in the manifest, for every server row of that run so the client language does not change mid-table. A raw `curl` row may be added; it does not replace the official-client row.

### 4.3 Embedded against server

This category is published only with an explicit caption:

> Embedded against server measures the cost of a process boundary and a protocol, together with the engine. It is not evidence that one engine's index is faster than the other's.

Permitted uses: showing what a caller pays to go from in-process Persia to the Persia server, and an explicitly labeled "how you would actually deploy it" note beside the matched categories. The matched categories (4.1 and 4.2) are the ones a claim about engine speed may cite. A claim that cites only this category must use the caption above in the same sentence.

Persia embedded against the Persia server, same machine, same corpus, same queries, is the cleanest reading of the category, because the index is the same family. Persia embedded against Elasticsearch (or any other server) is published only inside this category, with the caption, and never as the headline.

### 4.4 Comparisons that are not published

- A microbenchmark from [`docs/benchmarks.md`](./docs/benchmarks.md) placed next to another engine's end-to-end number.
- A SPEC §16.2 target placed next to a measured number as if both were measurements.
- An imported number from a vendor blog, a conference slide, or Rally's public results.
- A row whose correctness pass did not run.
- A row that mixes durability classes, cache conditions, or typo-tolerance settings.
- Multi-writer throughput. v1 is one writer per shard (SPEC §1.2, §8.4). The cell is `not applicable`, for every engine, including engines that allow several writers.

## 5. Competitors

Adapters are optional (ROADMAP 15.1.4) and external (ADR-0001, [`deny.toml`](./deny.toml)). The campaign can publish a Persia-only run. A Persia-only run is labeled `no-comparison`. It still follows correctness, statistics, and the reporting rules. It still is not a claim about another engine.

| Engine | Category | Equivalent-work notes |
|---|---|---|
| Tantivy | Embedded | Same documents, same queries, commit that syncs when the row is the durable row. Ranking is Tantivy's BM25, not BM25F; quality uses qrels, not score equality. |
| SQLite FTS5 | Embedded | `MATCH` plus `bm25()` when the row is ranked. Tokenizer and whether positions are stored (`detail`) are recorded. Phrase rows require positions. |
| Meilisearch | Server | Typo tolerance off for exact-match rows; on only for the typo row, with Persia relaxation on. The project documents typo tolerance as on unless disabled (see the [comparison guide](./website/src/content/docs/guide/comparison.md)). Re-read that default at pin time. Ranking rules recorded. |
| Typesense | Server | Same split as Meilisearch. Typesense documents typo tolerance as on by default (`num_typos` in its search API). Re-read the pinned version and record the value. |
| Elasticsearch | Server | One major.minor pin. Translog durability set for the durable row. Refresh interval recorded so "visible to search" means the same thing as a Persia commit. |
| OpenSearch | Server | Separate pin and config. Not a duplicate Elasticsearch column. |
| Solr | Server | Separate pin and config. |
| Persia embedded | Embedded | Production defaults unless the matrix is the recommended one. |
| Persia server | Server | SPEC §14 defaults. Block-cache size is the 15.2.4 default once that leaf sets it; until then the manifest records the bytes it used (`TBD`). |
| Persia Rust SDK, Persia Go SDK | Server, overhead row | SPEC §13.2, ROADMAP M14. |
| Embedded Go and TypeScript bindings | Out of the v1 campaign | ADR-0018, ROADMAP M16, after v1.0. A later amendment can add them. |

Refresh and visibility: Persia search sees the memtable after `put` returns (SPEC §7.3) and sees a commit after the manifest is published. A competitor that hides documents until a refresh must have that refresh inside the index workload's measured interval, and the query workload must run against a refreshed index. Otherwise the query is searching a smaller corpus. The planted-term check in the correctness pass is what enforces this.

## 6. Datasets

ROADMAP 15.1.1 is the acquisition work: public corpora, pinned versions, checksums. Nothing in this section has been downloaded by this plan. Names are the corpora the harness will pin. Licenses are checked at pin time from the publisher's own file, not from memory.

### 6.1 Size tiers

| Tier | Purpose | Size |
|---|---|---|
| Small | Correctness passes and a harness smoke test. | `TBD` in the 15.1.1 manifest. Generated in-repo when a public dump is too large or too encumbered to fetch in CI. |
| Medium | Cache behavior is visible. The working set is larger than the block cache configured for the run. | `TBD` bytes, chosen once the cache default exists (ROADMAP 15.2.4). The manifest states the byte size. The run states the cache size and the hit ratio. |
| Large | The scenario [SPEC §16.2](./SPEC.md#16-quality-requirements) names for its query and indexing targets: 10 million short documents, one node. | That count is the target scenario. It is not a corpus checked into the repository. The manifest confirms the actual document count after preprocessing (`TBD` until a run). |

A tier is selected before the run. Publishing the tier that looked best is cherry-picking.

### 6.2 Corpora

| Corpus | Why it is on the list | Judgments | License note |
|---|---|---|---|
| MS MARCO passages | Widely used passage-ranking corpus with queries. | Qrels on the official splits. | Microsoft's dataset license is research-oriented. Record the license file shipped with the dump. Do not redistribute. Not a CI default. |
| Wikipedia dumps | Large natural text. One edition per built-in analyzer language. | None by themselves. Pair with a judged query set where one exists. | Wikimedia dump terms. Article text is under the Creative Commons license the dump states (CC BY-SA at the time this plan was written; re-read at pin time). Attribute. Do not commit the dump. |
| BEIR subsets | Judged retrieval tasks. The campaign uses subsets, not "all of BEIR" as one blob. | Qrels per subset. | The BEIR framework's license is not the license of a subset. Record each subset. Skip a subset whose license forbids the use. |
| Stack Exchange dump (Stack Overflow and sibling sites) | Longer documents, title-as-query style workloads, a public dump. | Constructed relevance (title to its own post) is a weak label and is reported as constructed, separate from human qrels. | The dump's stated Creative Commons license (CC BY-SA on current dumps; record the dump's own statement). Attribute. |
| Hacker News | A public stream of short posts, useful as a head/tail and incremental-index workload if a dump can be pinned. | Usually none. | Posts are copyrighted by their authors. There is no single grant for the corpus. If the pin cannot record a license that allows the use, HN is excluded from published runs. It is never committed to git. |
| E-commerce product catalog | Facets, numeric filters, sorts. | Optional. | The default catalog is a deterministic synthetic generator with a recorded seed, so the campaign does not depend on a third-party catalog license. A public catalog is allowed only when its license file is recorded and permits the use. |
| Elastic Rally tracks (optional) | `geonames`, `http_logs`, the Stack Overflow track, and others, as dataset definitions other engineers already recognize. | Depends on the track. | Record the track's license. Rally's published timings are not imported. |

The AOL query log and any other leaked or privacy-sensitive query log are out. They are not pinned, not mirrored, and not used as a "public" head/tail source.

Constructed labels (a Stack Overflow title queried against its body, a planted term) support the correctness pass and a recall sanity check. They are not reported as nDCG. nDCG, MRR, and recall@k in the relevance section use human or official qrels only.

### 6.3 Multilingual set, including Spanish

Built-in analyzers today are `en`, `es`, `pt`, `fr`, `de`, and `it`, each at a versioned id (`crates/persia-analysis`). SPEC §6.3 is the contract. Spanish is a required member of the multilingual set, not an optional extra.

The multilingual suite is:

- A Wikipedia edition for each built-in language, Spanish (`eswiki`) included, indexed with that language's analyzer and the analyzer version recorded.
- A judged Spanish subset when one can be pinned. Candidates to evaluate at pin time, not pre-selected: the Spanish portion of MIRACL, and Spanish queries from mMARCO. Each brings its own license (mMARCO inherits MS MARCO's restrictions). If neither pin is lawful and practical, the Spanish Wikipedia suite still runs, and the relevance table says `no qrels` for that corpus instead of inventing judgments.
- The same query workload (term, phrase, prefix, typo) on each language, so a regression in one stemmer is visible. Typo generation is seeded and language-specific (accented characters stay in the source query; the analyzer folds what it folds). Preprocessing must not pre-stem or pre-fold.

Analyzer version strings are part of the pin. A Unicode bump that changes analysis (SPEC §6.3, ADR-0025) invalidates comparison with a run that used the previous version. Those two runs are different pins.

### 6.4 Licensing

Every dataset manifest has a `license` block: SPDX identifier when one applies, a URL of the publisher's terms, a one-line note of what the campaign is allowed to do (run locally, publish aggregate metrics, redistribute or not), and the date the terms were read.

Rules:

- Aggregate metrics may be published when the terms allow research or benchmark use.
- The corpus itself is not republished unless the terms allow it. The default is a URL plus a checksum, not a mirror in this repository.
- A dataset with no recorded license is not used in a published run.
- Non-commercial terms (expected for MS MARCO, confirmed at pin time) are compatible with publishing a methodology result. They are not compatible with shipping the corpus inside a commercial artifact. The manifest says which case it is.
- Synthetic data generated by the harness is Apache-2.0 with the repository and is the CI default.

### 6.5 Checksums and pins

Each dataset manifest records:

- Upstream URL and the version, dump date, or commit the publisher uses.
- SHA-256 of the upstream archive.
- SHA-256 of the canonical JSONL and of the query file after preprocessing.
- Document count, byte count, and the tier. These fields are filled when the archive is fetched. Until the first fetch they are `TBD`.
- Generator seed, when the corpus is synthetic.

The harness refuses to start a trial when a checksum mismatches. Checksums are not a substitute for the license block.

### 6.6 Preprocessing

One shared pipeline, run once per dataset version, produces the bytes every adapter reads. Adapters map those bytes onto an engine. They do not clean, truncate, or drop.

Canonical document (JSONL, one object per line):

- `id` (string)
- `fields` (object of strings and numbers the workload's schema names)
- optional `blob_path` for the blob workload, relative to the dataset root
- optional `lang`

Canonical queries (JSONL):

- `id`, `q` (the raw query string)
- optional `filter`, `sort`, `facet`, `page`
- optional `qrels` (list of `{id, grade}`)
- `bucket`: `head`, `tail`, `typo`, `exact`, or another label from the workload

Pipeline rules:

- HTML and markup stripping, if any, happens here, once. Engines do not see different text.
- Identifiers are stable across engines.
- Planted terms for the correctness pass are inserted here and listed in the manifest.
- The pipeline records its own version (git SHA) and the seed.
- Adapters must index every line. The correctness pass compares counts.
- Adapters must not parse a subset to save time. That is the terminal-emulator failure mode, and the planted-term check exists to catch it.

### 6.7 What is not stored in git

CLAUDE.md forbids committing large binary fixtures (the threshold there is 1 MB; generate them instead). Corpora, extracted JSONL, and raw histograms live outside git: a local cache directory named in the manifest, and for publication an artifact store addressed by checksum. Git holds manifests, configs, the result-record JSON when it is small, and checksums of the large artifacts.

## 7. Workloads

ROADMAP 15.1.2 names the first workload set: query logs (head, tail, typo-heavy, filtered, facet). This section is the full catalog the harness implements. Each workload id is stable. A workload definition is a file under the layout in [Directory layout](#102-directory-layout). It is data, not a measured result.

Shared outputs, unless a row says otherwise: the correctness block, the trial histogram, p50/p95/p99/p999 with intervals, the durability class, the cache condition, and the index byte size after the workload if the workload wrote data.

### 7.1 Catalog

| Id | Work | Category |
|---|---|---|
| `index.bulk` | Load the corpus from empty, durable commit. | Embedded and server, separate tables |
| `index.incremental` | Small batches over time, reader visibility after each batch. | Embedded and server |
| `commit.latency` | Time from commit/flush call to durable acknowledgement. | Embedded and server |
| `query.term` | Single analyzed term, top-k, scored. | Both matched categories |
| `query.phrase` | Phrase, positions on. | Both |
| `query.bool` | Boolean must/should/must_not. | Both |
| `query.prefix` | Prefix query, expansion cap recorded. | Both |
| `query.fuzzy` | Fuzzy/typo query with tolerance **on** for every engine. | Both |
| `query.filtered` | Term or boolean query plus a filter that is not relaxed. | Both |
| `query.facet` | Same, plus one facet field. | Both |
| `query.sorted` | Sort by a numeric field, and a separate sort-by-score row. | Both |
| `query.page` | Shallow window and deep window; cursor pagination where the engine has it. | Both |
| `query.head` | Most frequent queries from the dataset's own log. | Both |
| `query.tail` | Once-only queries from that log. | Both |
| `query.typo` | Seeded typos. Quality metrics required. | Both |
| `query.relax` | Persia relaxation cascade (M8) and the other engine's typo tolerance, both on. Quality metrics required. | Both |
| `mixed.rw` | One writer, several readers, ratio in the manifest. | Both |
| `scale.readers` | Increase reader concurrency. One writer. | Both |
| `update.upsert` | Upserts of existing ids. | Both |
| `update.delete` | Deletes, then search must not return them. | Both |
| `compact.impact` | Query latency while compaction runs, and while idle. | Both |
| `recover.wal` | `kill -9`, reopen, time to ready, documents match. | Embedded headline; server as a second table |
| `open.cold` | Process start to ready, and ready to first query, cold cache. | Embedded headline; server as a second table |
| `size.bytes` | Index bytes, and deployment bytes, after a quiescent commit. | Both |
| `mem.rss` | RSS after open, during query, peak during index. | Both |
| `blob.ingest` | Opaque blob write and ranged read (M9). | Embedded; server when M11 blob RPCs exist |
| `cloud.cache` | Object-storage backend, cold and warm, hit ratio (M12). | Server / storage. Not an embedded-library row |
| `dist.readers` | Shard count fixed, reader count scaled (M13). | Server only |
| `sdk.overhead` | Official SDK latency minus server-observed latency (M14). | Server only |

`TBD` inside a workload file (exact batch size, exact page offsets, reader counts) is chosen and committed before trials. The lists above are the ids the campaign owes, not a license to add ids after seeing numbers.

### 7.2 Indexing, commit, and flush

`index.bulk` reports documents per second and wall time, with the durable commit included in the interval. A bulk load that returns before the sync has finished is the buffered matrix, not this row.

`index.incremental` uses a batch size written in the manifest (`TBD`) and reports that size. Visibility is checked with a planted id from the batch just acknowledged.

`commit.latency` is the distribution of the commit call itself, durable class, not the time to analyze the whole corpus.

SPEC §16.2 names an indexing target of at least 50 thousand short documents per second for one writer, and an open target under 100 ms for a 1 GB local database. Those are targets. `index.bulk` on the large tier is the workload that will eventually be compared to the indexing target. The comparison is a result and is not in this file.

### 7.3 Query shapes

All of these return a top-k window. `k` is in the manifest (`TBD`), the same for every engine in the row. The SPEC §16.2 query targets (p50 under 10 ms, p99 under 50 ms, warm, 10 million short documents, one node) apply to the warm large-tier query rows once a run exists. They are not current measurements.

`query.phrase` enables positions on every engine. A participant that cannot index positions is `unsupported` for this id, and the others are still published.

`query.prefix` and `query.fuzzy` record the expansion cap. Persia's default cap is the SPEC §7.6 default in the table above. Setting a tighter cap on one engine only is a different workload.

`query.filtered` uses a predicate with a stated selectivity, computed by preprocessing and stored in the dataset (the fraction is `TBD` per dataset, measured once during preprocessing and then pinned, so it is a property of the data rather than a knob turned after a timing). Filters are not relaxed (SPEC §7.6) unless the row is explicitly `relax.filters = last_resort`.

`query.facet` asks for the same field and the same top-N (`TBD` in the manifest).

`query.sorted` has two sub-rows: sort by score, and sort by a numeric field. They are not averaged.

`query.page` has a shallow offset, a deep offset, and, where the engine supports it, a cursor (`search_after` on Persia, SPEC §7.4). Deep offset does more work than a cursor. An engine that rejects deep offsets is `unsupported` in that cell. `unsupported` is not a win, and the cell is not filled with a zero.

### 7.4 Query logs, typos, and relaxation

`query.head` and `query.tail` use frequencies from the dataset's query log. Head is the most frequent queries. Tail is a sample of queries that occur once. Both sets are frozen in the manifest before timing. A dataset with no frequency information does not get a synthetic head/tail built from observed latency. The id is `not applicable` for that dataset.

`query.typo` uses the shared generator from ROADMAP 8.4.1: edits applied to query strings, seed recorded, documents left clean. Every engine runs with typo tolerance on (Persia relaxation on, Meilisearch and Typesense typo tolerance on, Lucene-family fuzziness set to the same maximum edit distance the generator used). Exact-match rows are the opposite: Persia `relax: false`, typo tolerance off, fuzziness off.

`query.relax` is the product workload for M8. It reports latency and the ROADMAP 8.4.2 metrics together. The response's relaxation level is part of the raw record (SPEC §7.6). A run that disables relaxation and still uses this id is invalid. The budget (`relax.budget_ms`, default 50) is recorded because hitting the budget changes the result (`truncated: true`). Truncated and non-truncated trials are separate histograms, not one pool.

### 7.5 Mixed load, concurrency, updates, compaction

`mixed.rw` is one writer and a reader count from the manifest (`TBD`), with a write fraction from the manifest (`TBD`). Durability is `Fsync` on the headline. The buffered matrix may repeat this id with a label.

`scale.readers` varies reader count only. Writer count stays at one (SPEC §8.4). The counts are a list in the manifest, chosen before the run.

`update.upsert` and `update.delete` check correctness: after an acknowledged delete, a query must not return that id; after an acknowledged upsert, a fetch returns the new source. Throughput without that check is not published.

`compact.impact` reports query percentiles while a merge is running and while the engine is idle. Persia's trigger is the SPEC §8.5 policy; the workload builds a segment set that meets that policy, and does the analogous thing on each other engine (force a merge of a similar deleted fraction, fraction recorded). Background merges must not block reads or writes (SPEC §8.5). The measurement is whether latency moves, not a pass/fail invented here.

### 7.6 Recovery, open, size, and memory

`recover.wal` kills the process with `kill -9` after a WAL of a controlled size, then times reopen to ready (SPEC §8.6). The recovered set equals the acknowledged operations. One of the sizes is the SPEC §16.2 scenario (a 64 MiB WAL, target under 5 s, proportional to WAL size). At least one smaller and one larger size are included so the shape of the curve can be seen; those extra sizes are `TBD` in the manifest. This is wall-clock time on a real filesystem with a real sync. The Criterion `wal` bench is a different measurement ([Durability parity](#39-durability-parity)).

`open.cold` reports two intervals: process start to ready, and ready to first query result under the cold-cache rule. The SPEC §16.2 open target (under 100 ms for a 1 GB local database) refers to the first interval on a database of that size. Server cold start includes the server's own startup and is a server-category table.

`size.bytes` has two sub-rows. Index bytes are the files required to serve the corpus after a quiescent commit, temporary files excluded, paths listed. Deployment bytes add the binaries and configuration the category needs, and are not compared across the embedded-versus-server boundary as if they were index efficiency.

`mem.rss` samples the process resident set on a fixed interval recorded in the run (`TBD`), during open-idle, during the query set, and across index. Peak is the maximum sample, and the sampler's name is recorded. A sample interval that misses a spike is a limitation noted in the report, not a reason to pick the interval after the fact.

### 7.7 Blobs

`blob.ingest` writes opaque bytes and reads a range back (SPEC §9, ROADMAP M9). The bytes are not parsed as text. Extraction is out of scope (SPEC §1.2). If a workload indexes text about a blob, that text was produced in preprocessing and is the same string for every engine. Checksums of the bytes read back are part of the correctness pass.

### 7.8 Object storage

`cloud.cache` runs against the M12 backends. Providers: S3, GCS, and Azure Blob, each its own row (SPEC §10.2). CI and the smoke tier use the emulators the project already specifies for integration tests (MinIO, fake-gcs-server, Azurite; ROADMAP 12.2.4). A run on a commercial region is a separate labeled environment, with region, storage class, and endpoint recorded.

Cold and warm are both required (ROADMAP 12.4.7 for the MinIO pair). The result includes cache hit and miss counts. A first-query target in SPEC §16.2 (cold namespace, object storage, under 500 ms) is the target for the cold row on the scenario that leaf describes. It is not a measurement.

Local-disk numbers and object-storage numbers are different categories.

### 7.9 Sharded readers

`dist.readers` is server-only (ROADMAP M13, SPEC §11). Shard count is fixed at creation in v1 (SPEC §11.1) and is fixed in the manifest (`TBD`). The workload scales reader replicas. The router scatter-gathers (SPEC §11.2).

Sub-rows, not averaged:

- `global_stats: false` (shard-local statistics)
- `global_stats: true` (the pre-phase in SPEC §11.5)
- one shard, as the baseline
- the manifest's shard count

Partial failure (`partial: true`, ROADMAP 13.3.3) is its own sub-row. A partial result is not compared with a full result. Correctness for the full sub-row: with `global_stats: true`, results match the single-node reference (ROADMAP 13.3.6).

The report may draw a line for linear scaling as a visual reference. The line is not a claim that scaling is linear.

### 7.10 SDK overhead

`sdk.overhead` measures the Rust SDK and the Go SDK against one Persia server (SPEC §13.2, ROADMAP M14). The planned paths are `sdk/rust` (crate `persia-client`) and `sdk/go` (module `persia-go`). Neither directory exists yet. Embedded Go and TypeScript are outside this campaign (ADR-0018, ROADMAP M16).

Overhead is client-observed latency minus the latency the server reports for the same request, loopback, same queries as `query.term`. It is not embedded latency subtracted from SDK latency. That subtraction mixes categories. Both SDKs run the conformance scenarios (ROADMAP 14.1) before their timings are eligible, so a faster SDK that returns the wrong payload fails the correctness pass.

## 8. Relevance

Quality is a result of the same run as latency, on datasets that have official qrels. The metrics, chosen here as the reporting points (methodology, not a score that has been computed):

| Metric | Cut | Notes |
|---|---|---|
| nDCG | 10 | Also the metric SPEC §16.1 and ROADMAP 7.8.2 name for the relevance regression suite. |
| MRR | first relevant hit in the window | Window `k` matches the query workload. |
| recall | 10 and 100 | Both cuts are reported. Adding a cut is a manifest change before the run. |

Ranking configuration:

- Persia uses BM25F as specified (SPEC §7.2), with `k1`, `b`, and field boosts recorded. Headline defaults are 1.2, 0.75, and 1.0.
- Where the other engine exposes `k1` and `b`, the recommended matrix sets both engines from the vendor's documented defaults for that pin, and the defaults matrix sets Persia to SPEC defaults and the other engine to its defaults. The record includes both pairs.
- Score vectors are not required to match across engines. FTS5 `bm25()`, Lucene BM25, Tantivy BM25, and Meilisearch or Typesense ranking rules are different functions. Optional diagnostic: a correlation of scores on a shared boolean candidate set. It is not a pass/fail gate.
- Collection statistics are collection-level on Persia (SPEC §7.2). The sharded sub-row with `global_stats: false` is not pooled into the single-node nDCG number.

Relaxation quality (when the workload is `query.typo` or `query.relax`):

- Zero-result rate on the exact query.
- Zero-result rate after relaxation or typo tolerance.
- Precision of the first relaxed hit (ROADMAP 8.4.2).
- The fraction of trials that stopped because of the time budget, reported separately.

A higher nDCG and a higher latency are both published. The report does not collapse them into a single "better".

## 9. Environment

The publication machine is named in the manifest before trials. The first publication campaign's operating system is Linux, because that is the durability behavior SPEC §4.5.2 specifies most tightly (`fdatasync`). Other operating systems are additional labeled runs (macOS `F_FULLFSYNC` is a different cost; SPEC §4.5.2). They are not pooled with the Linux numbers.

Report these, and fail the run record if one is missing:

| Item | What is recorded |
|---|---|
| CPU | Model, sockets, cores, threads, base frequency, maximum frequency, SMT on or off, governor. The governor is pinned to `performance` for the session. Turbo is either on for every engine or off for every engine, and the choice is in the manifest. |
| Memory | Size, speed if known. |
| Disk | Model, whether it is rotational or flash, whether it is the OS disk, filesystem type, mount options. Not a RAM disk for any durability row. |
| OS | Distribution, kernel version. |
| Isolation | No other benchmark, no browser, no indexer. Record `uptime` and the set of processes above a CPU threshold at the start. A shared CI runner is not an isolation environment. |
| Virtualization | Bare metal, or a named hypervisor. |
| Cloud | Instance type, region, tenancy if dedicated. The same type for every re-run that claims to replicate. |
| Container | Separate category from bare metal. Image digest, cgroup CPU and memory limits, storage driver. An unlimited container and a limited one are different rows. |
| Clock | The sampler used for latency (the harness clock, not wall time from logs). |

Bare metal, or a dedicated instance of a recorded type, is the environment a latency claim may cite. A container run and a laptop run may be published. Their captions say what they are. They are not substituted into the bare-metal table.

Thermal drift is handled by alternating engines ([Reproducibility](#33-reproducibility)), not by assuming the machine is steady.

## 10. Harness

### 10.1 What M15.1 builds

| Leaf | Deliverable under this plan |
|---|---|
| 15.1.1 | Dataset manifests, fetch script, checksum verification, license block. |
| 15.1.2 | Workload files for every id in the [catalog](#71-catalog). |
| 15.1.3 | End-to-end runner for embedded Persia and the Persia server. HdrHistogram. Trial loop. Correctness pass before timing. |
| 15.1.4 | This document, plus optional external adapters and their config files. |
| 15.1.5 | The greater-than-5% regression gate on stored baselines for the in-repo Criterion suite, run on a quiet machine. Not the comparative campaign, and not the shared runner. |

Criterion benches stay where they are. They are the inputs to [`docs/benchmarks.md`](./docs/benchmarks.md) and to `.github/workflows/bench.yml`:

- `crates/persia-format/benches/` (`bitpack`, `blocks`, `compress`)
- `crates/persia-analysis/benches/analyzers.rs`
- `crates/persia-engine/benches/segment.rs`
- `crates/persia-engine/benches/wal.rs`
- `crates/persia-storage/benches/compaction.rs`
- `crates/persia-storage/benches/container_read.rs`

`just bench` runs one crate's Criterion benches (default `persia-engine`). That command is not the campaign.

### 10.2 Directory layout

SPEC §16.3 and CLAUDE.md name `benches/` as the harness directory. The root `bench/` name is not used: it would contradict the SPEC. Root `benches/` does not exist yet. Crate benches stay under `crates/<crate>/benches/` so Cargo's per-crate `benches/` and the campaign tree are not the same folder.

Proposed layout, created by the 15.1 implementation, not by this change:

```text
benches/campaign/                  # data and scripts. No Cargo.toml.
  README.md                        # points here and gives the one command
  datasets/                        # 15.1.1 manifests (urls, checksums, licenses). No corpus bytes.
  workloads/                       # 15.1.2, one file per workload id
  engines/                         # one directory per engine, config only
    persia-embedded/
    persia-server/
    tantivy/
    sqlite-fts5/
    meilisearch/
    typesense/
    elasticsearch/
    opensearch/
    solr/
  schema/result.schema.json        # Appendix A
  driver/                          # orchestration script invoked by xtask
```

Persia-side measurement is a workspace binary added in the 15.1.3 change (proposed name `persia-bench` under `crates/`). That change updates `ALLOWED` in `xtask/src/check_deps.rs`. The binary may depend on `persia` and `persia-server`. It may not depend on Tantivy, SQLite, or any other engine (ADR-0001, `deny.toml`).

`xtask` may depend only on `persia-format`. The campaign entry point in xtask is a process launcher (`cargo run -p persia-bench`, plus the external driver). It is not an in-process linking of the engines under test.

Competitor adapters are programs outside the Cargo workspace. The driver starts them. Product crates (`persia`, `persia-server`, `persia-cli`) do not spawn them. ADR-0001 forbids the product from taking a search engine as a dependency or invoking one as a subprocess. If a later change needs the workspace binary itself to spawn a competitor, that change amends ADR-0001 in the same pull request. It does not shell out quietly.

### 10.3 Engine configuration files

Each directory under `engines/` holds:

- `defaults.*` — the defaults matrix
- `recommended.*` — the recommended matrix, plus the URL of the vendor reply or guide
- `README.md` — the durability knob, the typo-tolerance knob, the refresh knob, and how stored fields and positions are turned on

The files are committed. Their checksums go into the result record. A run with an uncommitted local edit is not publishable.

### 10.4 Result record

One JSON object per (run, engine, workload, matrix, cache condition). The shape is [Appendix A](#appendix-a-result-record). Schema version starts at 1 when the harness is written. A breaking change of the schema bumps it. Records are append-only.

### 10.5 CI and regression tracking

| Job | What it does | What it must not do |
|---|---|---|
| `.github/workflows/bench.yml` (weekly, ROADMAP 1.11.2) | Criterion suite, artifact stored, compared with the previous successful artifact. | It does not fail the build on a 5% move today. Shared runners are too noisy. It does not run competitor engines. |
| ROADMAP 15.1.5 | Gate the Criterion suite at the SPEC §16.3 threshold once baselines live on a quiet machine. | It does not gate the comparative campaign. |
| Future harness smoke (`TBD`, part of 15.1.3) | Persia only, synthetic small tier, correctness pass, one trial. Proves the runner still starts. | It does not publish a number and does not fail on latency. |

Published campaign results are the baseline a later campaign compares against. The comparison is relative to a named run id, on the same machine class, with the same pins except the Persia SHA under test. A movement smaller than the confidence intervals is reported as undetected.

### 10.6 One command

Proposed, not implemented:

```text
just bench-campaign --manifest benches/campaign/runs/<run-id>.toml
```

The command fetches missing datasets, checks every checksum, runs the correctness pass, runs the trials, and writes the result records. It refuses to start if the manifest is dirty relative to git, if a pin is missing, or if the license block is empty.

What exists today is only:

```text
just bench
cargo bench -p <crate>
```

Those produce the snapshots in `docs/benchmarks.md`.

## 11. Reporting

### 11.1 Tables and plots

Each table's caption contains: run id, category, matrix (defaults or recommended), cache condition, durability class, dataset id and tier, and the machine label.

Column order is the engine order in the manifest, which is alphabetical within a category, Persia included in that order. Order is not "winner first".

Template, values intentionally empty:

| Workload | Engine | p50 (ms) | p50 95% CI | p99 (ms) | p99 95% CI | p999 (ms) | Correctness |
|---|---|---|---|---|---|---|---|
| `query.term` | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` | `TBD` |

Plots: one histogram per engine per workload (HdrHistogram), and a trial-scatter of the headline percentile so a reader can see the spread. Axes are labeled with units. A plot without the run id is not used in a claim.

Relevance tables are separate from latency tables. They share the run id so a reader can join them.

### 11.2 Raw data

Published with the report:

- The result records (Appendix A).
- Configuration files and their checksums.
- The dataset manifest (not the corpus, unless the license allows and the size rule allows).
- HdrHistogram logs, as artifacts addressed by checksum when they exceed the repository size rule.
- The correctness-pass output, including failed passes.
- The vendor-ask URLs and the replies.

A report that shows a table and withholds the raw record is not a publication under this file.

### 11.3 Run changelog

A single changelog, proposed path `benches/campaign/CHANGELOG.md` (created with the harness, not now), lists every run id. The repository [`CHANGELOG.md`](./CHANGELOG.md) is the product changelog and is not where bench runs go.

Each entry: date, run id, git SHA, manifest SHA, machine label, category, and a pointer to the records. Entries are appended. An entry is not edited to improve a number. A correction is a new entry that names the run it corrects.

### 11.4 How to challenge a result

Anyone may challenge a published run. The challenge names the run id, the workload id, and the equivalence rule that is believed broken (or the statistical rule, or the pin). A GitHub issue is enough. No form beyond that.

Maintainers respond by either explaining, with a pointer to the raw record, why the row meets the rule, or by publishing a correction entry. The original record stays. While a challenge is open, the run's summary says `disputed` and claims that depend on the disputed row stop until the correction entry lands.

A challenge that requires a re-run uses the same manifest and the same machine class. Changing the manifest is a new run, not a quiet replacement.

### 11.5 Review checklist

[Appendix B](#appendix-b-checklist-before-a-claim) is the checklist. A claim published without it is out of policy even if the number is accurate.

## 12. What may be claimed

A claim is allowed when all of the following are true:

1. M0–M15 are complete.
2. The claim cites a published run id, the git SHA, the workload id, the category, the matrix, the cache condition, and the dataset id.
3. The same page links the full table for that run, including workloads Persia lost, and the raw record.
4. The sentence's scope is no wider than the row. One corpus, one machine, one version, one percentile.
5. The interval is stated with the point.
6. Where intervals overlap, the sentence does not say Persia was faster or slower.

A claim looks like this, with the placeholders still to be filled by a real run:

> On run `TBD` (SHA `TBD`), embedded Persia p50 latency for `query.term` on dataset `TBD` (large tier) was `TBD` ms (95% CI `TBD`–`TBD`), and Tantivy `TBD` was `TBD` ms (95% CI `TBD`–`TBD`). Defaults matrix, warm cache, durable commit. Full table: `TBD`.

The following are not claims this project will make, and they are not implied by this file:

- Any statement that Persia is faster, slower, or "Nx" anything, until a run exists.
- A SPEC §16.2 target stated as an achieved latency.
- A figure from `docs/benchmarks.md` stated as a search-product result or as a comparison with another engine.
- A superlative ("fastest") that is not identical to a single row's interval comparison.
- A number from the embedded-against-server category without that category's caption in the same sentence.
- A screenshot of one trial, a laptop run, or an informal log line.
- A number whose run is marked `disputed`.

Marketing pages, the README comparison, and the website comparison guide may link to a run that meets this section. Until then they say that no search benchmark is published. This plan does not add a number to those pages.

## 13. Performance work in M15.2

ROADMAP 15.2 is how the project will respond to profiles. It is not a license to publish.

| Leaf | Relationship to this plan |
|---|---|
| 15.2.1 SIMD bit-unpacking | Justified by profiles and by the format benches in `docs/benchmarks.md` (the scalar rows are the baseline those benches already describe). A SIMD change lands with a new snapshot row there. |
| 15.2.2 Postings decode and scoring allocations | Justified by profiles of the query workloads. Allocation counts belong in tests. Timing belongs in a run. |
| 15.2.3 FST and term-dict layout | Justified by `size.bytes` and `mem.rss` profiles, not by a single lookup microbenchmark alone. |
| 15.2.4 Cache sizing defaults | The defaults this plan's server and cloud rows will pin. Chosen from `cloud.cache` and warm/cold query rows. |

A 15.2 change that its author wants to cite in public waits for a campaign run on the tagged tree. An internal before/after on one machine may be appended to `docs/benchmarks.md` as a snapshot, with the commit named, under the rules already at the top of that file.

## 14. Decisions left to the harness

These are intentionally `TBD`. They are chosen in the manifest before the first trial of the first campaign, then pinned. This file does not pretend they are known.

- Exact document counts and byte sizes for the small and medium tiers.
- The extra WAL sizes beside the 64 MiB SPEC scenario.
- Page offsets, facet top-N, reader counts, mixed-load ratio, batch size, result-window `k`.
- The wait for a vendor reply.
- The RSS sample interval.
- The block-cache byte size, until ROADMAP 15.2.4 sets the product default.
- Where large artifacts are uploaded (the checksum scheme above is the requirement; the host is an operational choice).
- Whether the first publication machine is owned hardware or a named cloud instance type.

Anything that would change the on-disk format is out of this plan. Format changes follow the SPEC, an ADR, a version or flag bump, and a golden fixture. A benchmark does not justify a silent format change.

## Appendix A. Result record

Proposed schema. The harness writes `benches/campaign/schema/result.schema.json` to match this. An example of the **shape** follows. Every measurement is `null` or `TBD`. This example is not a run.

```json
{
  "schema_version": 1,
  "run_id": "TBD",
  "registered_manifest_sha": "TBD",
  "persia_git_sha": "TBD",
  "rustc": "TBD",
  "status": "not_run",
  "category": "embedded-vs-embedded",
  "matrix": "defaults",
  "cache": "warm",
  "durability": "fsync",
  "engine": {
    "name": "TBD",
    "version": "TBD",
    "config_sha256": "TBD",
    "vendor_config_url": "TBD"
  },
  "dataset": {
    "id": "TBD",
    "tier": "TBD",
    "upstream_sha256": "TBD",
    "jsonl_sha256": "TBD",
    "license": "TBD",
    "doc_count": "TBD"
  },
  "workload_id": "TBD",
  "environment": {
    "cpu": "TBD",
    "governor": "performance",
    "ram": "TBD",
    "disk": "TBD",
    "os": "TBD",
    "kernel": "TBD",
    "filesystem": "TBD",
    "virtualization": "TBD",
    "instance_type": "TBD"
  },
  "correctness": {
    "passed": null,
    "reference_epsilon": "TBD",
    "doc_count_match": null,
    "planted_terms_found": null,
    "recall_at_k": null
  },
  "trials": null,
  "histogram_sha256": null,
  "latency_ms": {
    "p50": null,
    "p50_ci95": null,
    "p95": null,
    "p99": null,
    "p99_ci95": null,
    "p999": null,
    "max": null
  },
  "quality": {
    "ndcg_at_10": null,
    "mrr": null,
    "recall_at_10": null,
    "recall_at_100": null,
    "zero_result_rate_before": null,
    "zero_result_rate_after": null
  },
  "notes": "TBD"
}
```

`schema_version` is the version of this JSON shape, not a format version of the database and not a score.

SPEC §16.2 targets, copied here so a future report can point at the scenario. **Targets, not measurements, not promises:**

| Metric | Target |
|---|---|
| Query p50 (warm, 10M short docs, 1 node) | < 10 ms |
| Query p99 (warm) | < 50 ms |
| Cold namespace first query (object storage) | < 500 ms |
| Indexing throughput (1 writer) | ≥ 50k docs/s (short docs) |
| Open time (1 GB db, local) | < 100 ms |
| Recovery after crash | proportional to WAL size, < 5 s for 64 MiB WAL |

## Appendix B. Checklist before a claim

- [ ] M0–M15 are complete, and the binary is the SHA in the run record.
- [ ] The run id is in the campaign changelog and was registered before the first trial.
- [ ] Correctness passed for every engine in the cited row. Failed engines are in the table.
- [ ] Category, matrix, cache, and durability are in the sentence.
- [ ] The percentile and the 95% interval are in the sentence.
- [ ] Losses from the same run are linked beside the sentence.
- [ ] Raw result records are linked.
- [ ] No figure is a SPEC target, a `docs/benchmarks.md` snapshot, or a vendor's published number.
- [ ] No cited row is `disputed`.
- [ ] Embedded-against-server citations include that category's caption.
- [ ] Dataset license allows publication of aggregate metrics.
- [ ] A second person has ticked this list. The author of the run does not tick it alone.

## Appendix C. Equivalence checklist

Tick per row, per engine:

- [ ] Source JSONL checksum matches the manifest.
- [ ] Document count matches the source.
- [ ] Planted terms are found.
- [ ] Stored source text matches on the id sample, or the row does not fetch stored fields for anyone.
- [ ] Query strings, filters, sort, and `k` match the workload file.
- [ ] Scoring is on if the workload says ranked.
- [ ] Positions are on if the workload has a phrase.
- [ ] Typo tolerance is the same side of on/off for every engine.
- [ ] Durability class matches the row.
- [ ] Cache condition matches the row.
- [ ] Refresh or commit completed before queries, and the planted id from the last batch is visible.
- [ ] Expansion caps are recorded and equal, or the difference is the point of two separate rows.
- [ ] The adapter does not drop, truncate, or skip documents.
- [ ] The adapter's config file checksum is the one in the result record.
- [ ] v1 non-goals are `not applicable` for every engine, not a blank Persia cell.
