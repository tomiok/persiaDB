# Persia DB

[![CI](https://github.com/tomiok/persiaDB/actions/workflows/ci.yml/badge.svg)](https://github.com/tomiok/persiaDB/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)
[![Documentation](https://img.shields.io/badge/docs-persiaDB-0c5c6b)](https://tomiok.github.io/persiaDB/)

Persia DB is an embedded, single-file full-text search database written in Rust ("SQLite for search"). It is pre-alpha. The design is to open one file, index documents, and return ranked results that, by default, never come back empty. The same segment bytes are specified to live in a local file or in object storage (S3, GCS, or Azure Blob) and to be served by stateless query nodes. What the repository implements today includes on-disk codecs and the local-file storage backend, as library code. The `persia` crate exports no `Db` or `Collection`, and nothing in the repository runs a query. Read the [documentation](https://tomiok.github.io/persiaDB/), the [comparison with SQLite FTS5, Tantivy, and Meilisearch](https://tomiok.github.io/persiaDB/guide/comparison/), and the [FAQ](https://tomiok.github.io/persiaDB/guide/faq/).

> **Status: pre-alpha.** Nothing works end-to-end yet, and the on-disk format is unstable until v1.0.
> See [`PROGRESS.md`](./PROGRESS.md) for what is done.

## Why

- **Zero friction:** `Db::open("app.persia")` just works. No daemon, no config files.
- **Own engine:** the index format, analysis, BM25F scoring and query execution are written from scratch.
  No Tantivy, Lucene, SQLite or RocksDB underneath.
- **Never zero results:** when a query matches nothing, Persia relaxes it step by step (synonyms, dropped terms,
  prefix, typo tolerance) and tells you exactly what it changed.
- **Cloud-native:** immutable segments, object-storage friendly, stateless query nodes.
- **Correctness first:** designed for checksummed data, crash-safe commits and fuzzed parsers.

## Quickstart

_Coming with the first release (`v0.1`, ROADMAP M10)._ A sketch of the intended shape; method names are
illustrative and not final:

```rust,ignore
let db = persia::Db::open("app.persia")?;
// Collections declare their schema up front (SPEC §6.2).
let products = db.create_collection(schema)?;
products.put(doc)?;
products.commit()?;

// No exact match for the typo? The query is relaxed (here: fuzzy, SPEC §7.6),
// and the response says what changed: "runing" -> "running".
let results = products.search("runing")?;
```

## Documentation

| Document | What it is |
|---|---|
| [Documentation](https://tomiok.github.io/persiaDB/) | Project site: guides, status, and the intended API |
| [`docs/guide.md`](./docs/guide.md) | User-facing guide (source for the project website): concepts, planned features, milestone status |
| [`website/`](./website/) | Project website (Astro Starlight), deployed to GitHub Pages |
| [Getting started](https://tomiok.github.io/persiaDB/guide/getting-started/) | What you can call today, and the intended API (illustrative where it is not implemented) |
| [llms.txt](https://tomiok.github.io/persiaDB/llms.txt) | Index of the site for agents. [llms-full.txt](https://tomiok.github.io/persiaDB/llms-full.txt) is the pages in Markdown |
| [`SPEC.md`](./SPEC.md) | The design: file format, segments, search, relaxation, storage, distribution |
| [`ROADMAP.md`](./ROADMAP.md) | All planned work; every leaf is one issue |
| [`PROGRESS.md`](./PROGRESS.md) | Generated status graph of the roadmap |
| [`CLAUDE.md`](./CLAUDE.md) | Engineering rules for contributors (human or AI) |
| [`docs/architecture.md`](./docs/architecture.md) | Map of the code: layers, crates, write and query paths |
| [`docs/adr/`](./docs/adr/) | Architecture decision records |
| [`docs/sdk/`](./docs/sdk/README.md) | Design for the embedded Go and TypeScript SDKs (ADR-0018). Not implemented. |

## License

Licensed under the [Apache License, Version 2.0](./LICENSE).
