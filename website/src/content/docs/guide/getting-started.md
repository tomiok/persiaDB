---
title: "Getting started with PersiaDB"
description: "Getting started with PersiaDB, a Rust search library aimed at embedded full-text search. The embedded Db API is planned. Format-layer codecs, storage, schema, and analysis are what you can call today. Nothing runs a query."
head:
  - tag: title
    content: "Getting started with PersiaDB: a Rust search library"
  - tag: meta
    attrs:
      property: og:title
      content: "Getting started with PersiaDB: a Rust search library"
---

Getting started with PersiaDB means separating this Rust search library's design from the code you can call.
The design is embedded full-text search in Rust: a library in your process, one file, ranked results, and no search daemon.
That embedded `Db` API is planned (roadmap M10) and is not in the crate.
What you can call today starts with the format-layer codecs in `persia-format`, plus the storage, schema, and analysis libraries below.
Nothing indexes a document or runs a query.

[What is an embedded search database?](/guide/embedded-search/) defines that in-process shape.
The [comparison](/guide/comparison/) places the design next to SQLite FTS5, Tantivy, and Meilisearch, and it is not a benchmark.
The [FAQ](/guide/faq/) has the short answers.
When this page and the [SPEC](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) disagree, the SPEC wins.
Live status is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md): %%PROGRESS_SUMMARY%%.
The same snapshot, by milestone, is the [roadmap](/roadmap/).

## What this Rust search library implements today

The crates are not published (`publish = false` in the workspace). There is no crates.io release and no `cargo add persia`.

`persia`, the embedded API crate, exports no types. Its source is the module docs in [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs). There is no `Db` and no `Collection`.

`persia-server` is an empty `main` function. No gRPC or HTTP server runs.

`persia-cli` lists and removes temporary and quarantine files next to a container or a local store (`persia container files`, `persia container sweep`, `persia store files`, `persia store sweep`). It does not inspect an index, dump documents, verify segments, compact, import, or search.

`persia-format` encodes and decodes on-disk bytes. It does not open a file as a database. A git dependency, for code that wants those codecs:

```toml
[dependencies]
persia-format = { git = "https://github.com/tomiok/persiaDB" }
```

This round-trips a container header (SPEC §4.2). It is the doctest on [`Header`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/header.rs). It does not create `app.persia` and it does not search:

```rust
use persia_format::header::{Access, HEADER_LEN, Header};
use persia_format::{Reader, Writer};

let header = Header::new([7; 16]);
let mut w = Writer::new();
header.encode(&mut w)?;
assert_eq!(w.len(), HEADER_LEN);

let decoded = Header::decode(&mut Reader::new(w.as_slice()))?;
assert_eq!(decoded, header);
assert_eq!(decoded.access(), Access::ReadWrite);
```

Also implemented in that crate, and only as byte codecs: checksums, integer blocks, zstd block compression, block framing, the two superblock slots, frames (including the streaming encoder), the memory-map reader, and `BlobId` / `BlobRef`. The public module list is [lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs). The [code](/guide/code/) page quotes exports from that file.

`persia-storage` implements the `Storage` trait, a memory backend, a local-file backend, and the single-file container: create, open, append, commit, recover, and compact. That commit is the container file, not an index of documents. `persia-engine` implements collection schema, document validation, and manifest types. It does not build a segment or answer a query. `persia-analysis` implements tokenizers, filters, English, Spanish, and Portuguese stemmers, and the built-in analyzers `standard`, `keyword`, `whitespace`, `en`, `es`, and `pt`. It does not search. `persia-blob` exports no API.

Those crates are in the same git repository and are unpublished the same way. Depending on them does not give you `Db::open`.

## Embedded full-text search in Rust

**Illustrative. Not implemented.** Nothing in this section is a function you can call. Names match the README sketch and SPEC §6.2, §7.1, §7.6, §8.1, and §9.6. They are not a stability promise. The first release that is supposed to provide them is `v0.1` (ROADMAP M10), which is blocked on the write path, query, relaxation, and blobs.

The shape a caller is meant to use:

```rust
// ILLUSTRATIVE — these methods are not in the persia crate.
let db = persia::Db::open("app.persia")?;

// Schema is declared up front (SPEC §6.2). Adding a field later is allowed.
// Changing a field's type is not.
let products = db.create_collection(schema)?;

products.put(doc)?;
products.commit()?;

// A typo still returns hits. The response says what changed (SPEC §7.6).
let results = products.search("runing")?;
let relaxation = &results.relaxation;
```

The collection schema in SPEC §6.2, as JSON. This is the design, not a parser that exists:

```json
{
  "name": "products",
  "fields": {
    "title": { "type": "text", "analyzer": "en", "boost": 3.0 },
    "body": { "type": "text", "analyzer": "en" },
    "brand": { "type": "keyword", "facet": true },
    "price": { "type": "f64" },
    "sales": { "type": "i64" }
  },
  "ranking": { "static_rank": { "field": "sales", "weight": 0.1, "fn": "log1p" } },
  "relaxation": { "enabled": true, "min_results": 1, "levels": "default", "fallback": "static_rank" }
}
```

Field types in that schema are `text`, `keyword`, `i64`, `f64`, `bool`, `datetime`, and `stored` (SPEC §6.1). Every document also has a string `id` (at most 512 bytes, unique in the collection). Documents are submitted as JSON, or as a `serde` struct via a planned `put_struct`, and stored as CBOR (SPEC §6.4).

`put` is an upsert by id. `delete` writes a tombstone. `commit` publishes a manifest. A `put` is specified to be visible to a later search on that database before commit (read-your-writes, SPEC §8.2–§8.4). One writer; a second writer fails immediately. Handles are specified as `Send` and `Sync`.

Search is specified to accept a text query or a structured query (SPEC §7.1): fields and boosts, filters, sort, `limit` / `offset`, facets, and `relax`. Scoring is BM25F plus an optional static rank (SPEC §7.2).

### Relaxation

**Planned (M8).** Default on. If a query has fewer than `min_results` hits (default 1), the engine walks a cascade and stops at the first level that has enough hits (SPEC §7.6):

| Level | Name | What changes |
|---|---|---|
| L0 | `exact` | Every analyzed term is required |
| L1 | `synonyms` | Collection synonyms, OR within a group |
| L2 | `drop_terms` | Require fewer terms, dropping low-IDF terms first |
| L3 | `prefix` | Prefix-expand terms of length at least 3 |
| L4 | `fuzzy` | Edit distance 1 or 2, and report the corrected terms |
| L5 | `fallback` | `static_rank`, or `none` |

Filters are not relaxed unless the caller sets `relax.filters` to `"last_resort"`. `relax: false` on a query turns the cascade off.

The response is specified to carry what happened. This object is from SPEC §7.6. No function returns it today:

```json
{
  "level": "fuzzy",
  "applied": ["drop_terms:75%", "fuzzy"],
  "original": "zapatilas rojs",
  "effective": "zapatillas rojas",
  "corrected": { "zapatilas": "zapatillas", "rojs": "rojas" }
}
```

### Command line

**Planned (ROADMAP 10.4), except the file-sweep commands above.** The `persia` binary is specified to grow `inspect`, `dump`, `verify`, `compact`, `stats`, JSONL import, a one-shot search, and a small bench. The binary in the tree does none of those.

## Common questions

The [FAQ](/guide/faq/) answers the same kind of question for status, how this differs from SQLite FTS5, Tantivy, and Meilisearch, the file format, crash safety, SDKs, and the license.

### Can I add Persia to a Rust project today?

Not as a database. `persia` is unpublished and has no API. You can depend on `persia-format` from git, as shown above, if you need the byte codecs, and on `persia-storage`, `persia-engine`, and `persia-analysis` for the container, schema types, and analyzers. None of those dependencies give you `Db::open`.

### How do I open a database, index, and search?

You don't, yet, through `Db::open`. Those calls are the illustrative block on this page. A lower-level container open exists in `persia-storage`. It does not index or search. The milestone that is supposed to ship the embedded API is M10 (`v0.1`). It waits on the document write path, query, relaxation, and blobs. [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) is the checklist.

### How do I read which relaxation was applied?

On the intended API, from the search response field `relaxation` (SPEC §7.6): `level`, `applied`, `original`, `effective`, and `corrected`. Stricter levels rank above looser ones. Nothing in the repository computes this.

### Is the on-disk format stable?

No. Format version 1.0 is a draft until the v1.0 release (SPEC §15). There is no compatibility promise. A change to the bytes requires a SPEC edit, a version or feature-flag bump, an ADR, and a golden fixture.

### Is there a server, a cloud backend, or a client SDK?

Planned, not started. gRPC and HTTP are M11. S3, GCS, and Azure Blob are M12. Kubernetes sharding is M13. Rust and Go network clients are M14. Embedded Go and TypeScript bindings, which would open a file in-process, are a separate design ([Embedded SDKs](/guide/sdks/)); they are not implemented either. Embedded mode, when it exists, has no authentication and no config file (SPEC §14).

### Why is a query supposed to avoid empty results?

That is a product rule (SPEC §7.6), not an optional plugin. A query that matches nothing is relaxed step by step, and the response says what changed. It is not implemented.

### Where is the source of truth?

[SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) for behavior. [ROADMAP.md](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md) for planned work. [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) for what is done. This site is a reading of those files. [For AI agents](/guide/for-agents/) is the short version of the same constraints.
