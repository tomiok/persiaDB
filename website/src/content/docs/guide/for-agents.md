---
title: "For AI agents"
description: "Status, the API that exists, the API that is only specified, and the constraints for code that reads or writes this repository."
---

Read this before generating a call to Persia or a patch against the repo.
The design contract is [SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md).
If a page on this site disagrees with the SPEC, the SPEC wins.
Machine-readable copies of these pages: [llms.txt](/llms.txt) and [llms-full.txt](/llms-full.txt).

## Status

Pre-alpha. %%PROGRESS_SUMMARY%%.
The live counter is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md). Do not treat a number on an older page as newer than that file.

Nothing works end to end. You cannot open a database or search.
Opening a container file is implemented in `persia-storage`; that is not `Db::open`, and it does not search.
`SegmentBuilder` and `SegmentWriter` in `persia-engine` can write one segment's bytes. That is not `put` or `commit`, and nothing searches it.
The on-disk format is unstable until v1.0 (SPEC §15).

| Question | Answer |
|---|---|
| Usable database API? | No. `persia` exports no types. |
| Published on crates.io? | No. `publish = false`. |
| CLI? | File sweep (`container` / `store` `files` and `sweep`), plus `inspect segment` and `dump segment` (`--verify` on inspect). Not compact, import, or search. |
| Working library code? | `persia-format` codecs (header, superblock, checksums, integer codecs, block compression, block framing, frames, mmap, `BlobId`). `persia-storage`: memory, local file, and the single-file container. `persia-engine`: schema, documents, manifests, and a segment builder, writer, and reader. `persia-analysis`: tokenizers and built-in analyzers (`standard`, `keyword`, `whitespace`, `en`, `es`, `pt`, `fr`, `de`, `it`). |
| Next work? | The ready-next list in PROGRESS.md. |
| First callable release? | `v0.1`, ROADMAP M10. Blocked on the write path, query, relaxation, and blobs. |

The walkthrough with the illustrative calls, and the FAQ, is [Getting started](/guide/getting-started/).

## Do not invent these calls

The following names appear in the SPEC and the README. They are **not** Rust items in this repository. Do not emit them as working code, and do not describe them as available:

`Db::open`, `create_collection`, `put`, `put_batch`, `put_struct`, `delete`, `get`, `commit`, `search`, `put_blob`, `get_blob`, `OpenOptions`.

A search result's `relaxation` object (`level`, `applied`, `original`, `effective`, `corrected`) is specified in SPEC §7.6 and returned by nothing today.

There is no Go module and no npm package. `sdk/go` in the SPEC is a future gRPC client. The embedded Go and TypeScript design ([Embedded SDKs](/guide/sdks/), [ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md)) is a document, not a library. Do not emit `import "github.com/tomiok/persiaDB/sdk/persia"` or `import { open } from "@persiadb/node"` as working code.

Code that only needs the byte codecs can use `persia-format` (`Header`, `Reader`, `Writer`, `BlobId`, `BlobRef`, and the modules listed in [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs#L10-L40)). Quote those from the source. The [code](/guide/code/) page links the line ranges.

## Constraints on patches

[CLAUDE.md](https://github.com/tomiok/persiaDB/blob/main/CLAUDE.md) is the engineering ruleset. The short list:

- The engine is written here. Do not add Tantivy, Lucene, SQLite, RocksDB, or another search or storage engine.
- `unsafe` is allowed only in files listed in `xtask/src/repo_checks.rs` (`UNSAFE_ALLOWED`): `persia-format`'s mmap and the container's mapped-read module. Each block needs a `SAFETY` comment and an ADR. ADR-0018 reserves one future module, the C exports of `persia-ffi`, which is not in the tree.
- Bytes from disk, network, or callers return `Err`, never a panic. Corruption is `Error::Corrupt`.
- A change to on-disk layout updates SPEC.md, bumps a version or feature flag, adds an ADR, and adds a golden test.
- Segments are immutable once written. Data is written first, the commit record last, then fsync.
- Dependencies point downward. The allowed list is the workspace `[workspace.dependencies]` and `cargo xtask check-deps`.
- One roadmap leaf is one change. Do not mark a leaf done until its definition of done is met.

## How to read the docs

Each guide page has a `description` in its frontmatter and stable headings.
[llms.txt](/llms.txt) is the index. [llms-full.txt](/llms-full.txt) is the pages in Markdown.
Prefer those, plus SPEC.md, over guessing from the crate names.
`persia-engine`, `persia-storage`, and `persia-analysis` export the library code named above, including the segment builder. `persia-blob` and `persia-proto` are module docs only.
