# Persia DB

[![CI](https://github.com/tomiok/persiaDB/actions/workflows/ci.yml/badge.svg)](https://github.com/tomiok/persiaDB/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](./LICENSE)

**An embedded, single-file, cloud-native search database, written in Rust. Think "SQLite for search".**

Open a file, index documents, and get ranked results that, by default, never come back empty.
The same on-disk format can be served from object storage (S3 / GCS / Azure) and scaled out on Kubernetes.

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
| [`SPEC.md`](./SPEC.md) | The design: file format, segments, search, relaxation, storage, distribution |
| [`ROADMAP.md`](./ROADMAP.md) | All planned work; every leaf is one issue |
| [`PROGRESS.md`](./PROGRESS.md) | Generated status graph of the roadmap |
| [`CLAUDE.md`](./CLAUDE.md) | Engineering rules for contributors (human or AI) |
| [`docs/adr/`](./docs/adr/) | Architecture decision records |

## License

Licensed under the [Apache License, Version 2.0](./LICENSE).
