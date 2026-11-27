---
title: "For AI agents"
description: "Status, the public embedded API to call, what is only specified, and the constraints for code that reads or writes this repository."
head:
  - tag: title
    content: "API status and repository rules for AI agents | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "API status and repository rules for AI agents | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "API status and repository rules for AI agents | PersiaDB"
---

Read this before generating a call to Persia or a patch against the repo.
The design contract is [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md).
If a page on this site disagrees with the SPEC, the SPEC wins.
Machine-readable copies of these pages: [llms.txt](/llms.txt) and [llms-full.txt](/llms-full.txt).

## Status
## Status

Pre-alpha. %%PROGRESS_SUMMARY%%.
The live counter is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md). Do not treat a number on an older page as newer than that file.

%%PRE_ALPHA%%
The on-disk format is unstable until v1.0 (SPEC §15).

| Question | Answer |
|---|---|
| Usable database API? | Yes, as a Git dependency: the `persia` crate (`Db`, `Collection`, `SearchRequest`). Its surface is [`crates/persia/public-api.txt`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/public-api.txt). |
| Published on crates.io? | No. `publish = false`. Depend on the Git repository. |
| CLI? | `persia import`, `search`, `gc`, `verify`, `stats`, `compact`, `inspect`, `dump`, `container files` / `sweep`, and `bench`. |
| Working library code? | %%LIBRARY_NOW%% |
| Next work? | The ready-next list in PROGRESS.md. |
| Embedded API milestone? | %%M10_BLOCKED%% |

The walkthrough, and the FAQ, is [Getting started](/guide/getting-started/).

## The calls to emit

Generate calls to `persia` only: `Db::open` (or `OpenOptions::open`), `Db::create_collection_json` or `Db::create_collection`, `Db::collection`, `Collection::put_json`, `Collection::put`, `Collection::put_struct`, `Collection::get`, `Collection::delete`, `Collection::commit`, `Collection::search` with a `SearchRequest`, `Collection::search_json` with a request JSON (SPEC §7.1), `Db::put_blob` and `Collection::put_with_blob` for files, and `Db::close`. Every call returns `persia::Result`; branch on `Error::code` (an `ErrorCode`), never on the message text. Every name in [`public-api.txt`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/public-api.txt) is callable; nothing else is. Do not emit the engine crates (`persia-engine`, `persia-blob`, `persia-storage`, `persia-format`) or the hidden `internal` module as API: they are not stable and may change without notice.

A search response's `relaxation` (a `Relaxation`) says how the query was relaxed (SPEC §7.6): `level`, `applied`, `original`, `effective`, `truncated`, and the getters `corrected()`, `fallback()` (when L5 found the hits), and `filters_dropped()` (when L6 is the hit step). `corrected()` is set when L4 is the step that found the hits (ROADMAP 8.2.4.3, 8.1.22). Do not describe a typo as rewritten when an earlier step found the hits. L1 through L5 build a step. L5 ignores the request's `sort` for its ranking and flags that order with `fallback()`. With `relax.filters: "last_resort"`, one last step (L6) drops the request's `soft_filter` when every earlier level found fewer than `min_results` hits; the hard `filter` is never dropped. Queries that search no word do not plan it. The response reports `level: "filters"` and `filters_dropped: true` only when L6 supplies the hits (ROADMAP 8.2.6). Do not describe such hits as matching the soft filter. %%RELAXATION_NOW%% Each `Hit` names the level that found it (`level`). [BM25F ranking](/guide/ranking/) is the score: field weights, statistics, static rank on L5, bounds, and proximity off unless `SearchRequest::proximity`. [Faceted search](/guide/faceted-search/) is filters, facets, sort, cursors, deadlines, and explain. An explained hit of a relaxed search has a root `Explanation::Relaxed` naming the level and the explanation under it (ROADMAP 8.3.4). [Typo-tolerant search](/guide/typo-tolerant-search/) is the cascade, including `minimum_should_match` and how terms are dropped. [Fuzzy search](/guide/fuzzy-search/) is L4, the correction, L5, typo tiers, relax options, and the filter step. [Synonym and prefix search](/guide/synonym-and-prefix-search/) is L1 and L3. Do not say L4 fuzzy or L5 fallback is unbuilt.

## Do not invent these calls

There is no Go module and no npm package. `sdk/go` in the SPEC is a future gRPC client. The embedded Go and TypeScript design ([Embedded SDKs](/guide/sdks/), [ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md)) is a document, not a library. Do not emit `import "github.com/tomiok/persiaDB/sdk/persia"` or `import { open } from "@persiadb/node"` as working code. There is no server: `persia-server` exits with "not implemented (M11)", so do not emit gRPC or HTTP calls to one. `s3://`, `gs://`, and `az://` locations return `ErrorCode::Unsupported` (object storage is M12).

## Constraints on patches

[CLAUDE.md](https://github.com/tomiok/persiaDB/blob/main/CLAUDE.md) is the engineering ruleset. The short list:

- The engine is written here. Do not add Tantivy, Lucene, SQLite, RocksDB, or another search or storage engine.
- `unsafe` is allowed only in files listed in `xtask/src/repo_checks.rs` (`UNSAFE_ALLOWED`): `persia-format`'s mmap and the container's mapped-read module. Each block needs a `SAFETY` comment and an ADR. ADR-0018 reserves one future module, the C exports of `persia-ffi`, which is not in the tree.
- Bytes from disk, network, or callers return `Err`, never a panic. Damaged bytes are a corruption error (`ErrorCode::Corrupt` at the `persia` API).
- A change to on-disk layout updates SPEC.md, bumps a version or feature flag, adds an ADR, and adds a golden test.
- Segments are immutable once written. Data is written first, the commit record last, then fsync.
- Dependencies point downward. The allowed list is the workspace `[workspace.dependencies]` and `cargo xtask check-deps`.
- One roadmap leaf is one change. Do not mark a leaf done until its definition of done is met.
- A Rust sample on a guide page is a whole `fn main` program that `cargo test -p persia-docs` runs, and no page names an item `public-api.txt` lacks (`cargo test -p xtask`).

## How to read the docs

Each guide page has a `description` in its frontmatter and stable headings.
[llms.txt](/llms.txt) is the index. [llms-full.txt](/llms-full.txt) is the pages in Markdown.
Prefer those, plus SPEC.md and `public-api.txt`, over guessing from the crate names.
The engine crates under `persia` are described on [Crate architecture](/guide/architecture/) and quoted on the [code](/guide/code/) page, as internals. [Blob storage](/guide/blobs/) is the blob API (`Db::put_blob`, `get_blob`, `get_blob_range`, `Collection::put_with_blob`). `persia-proto` is module docs only.
