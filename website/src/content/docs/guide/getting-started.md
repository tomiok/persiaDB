---
title: "Getting started with PersiaDB"
description: "Getting started with PersiaDB, a pre-alpha embedded search database for Rust: add the dependency, open a .persia file, index documents, search them, and use the persia CLI."
head:
  - tag: title
    content: "Getting started with PersiaDB: a Rust search library"
  - tag: meta
    attrs:
      property: og:title
      content: "Getting started with PersiaDB: a Rust search library"
---

PersiaDB is embedded full-text search in Rust: a library in your process, one `.persia` file, ranked results, and no search daemon.
This page shows the public API of the `persia` crate as it is on `main`, and the `persia` command-line tool.

Persia DB is pre-alpha. There is no release, and the on-disk format is unstable until v1.0, with no compatibility promise (SPEC §15).
%%M10_BLOCKED%%
Live status is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md): %%PROGRESS_SUMMARY%%.
The same snapshot, by milestone, is the [roadmap](/roadmap/).

[What is an embedded search database?](/guide/embedded-search/) defines the in-process shape.
The [comparison](/guide/comparison/) places the design next to SQLite FTS5, Tantivy, and Meilisearch, and it is not a benchmark.
The [FAQ](/guide/faq/) has the short answers.
When this page and the [SPEC](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) disagree, the SPEC wins.

## Add Persia to a Rust project

The crates are not published (`publish = false` in the workspace). There is no crates.io release and no `cargo add persia`. Depend on the Git repository:

```toml
[dependencies]
persia = { git = "https://github.com/tomiok/persiaDB" }
```

## Open a database, index, and search

This is the [README quickstart](https://github.com/tomiok/persiaDB#quickstart), with the database in a temporary directory (the `tempfile` crate) instead of `app.persia` in the working directory; any path works:

```rust
use persia::{Db, SearchRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("app.persia"))?; // creates app.persia if it is missing
    let notes = db.create_collection_json(
        r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#,
    )?;
    notes.put_json(r#"{"id": "n1", "body": "Hello, world"}"#)?;
    notes.put_json(r#"{"id": "n2", "body": "hello, hello again"}"#)?;
    notes.put_json(r#"{"id": "n3", "body": "goodbye"}"#)?;
    notes.commit()?;

    // Ranked with BM25F: "n2" says hello twice, so it ranks first. "goodbye" does not match.
    let page = notes.search(&SearchRequest::new("hello"))?;
    let ids: Vec<_> = page.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["n2", "n1"]);

    // A typo still returns hits, and the response says how the query was relaxed.
    let page = notes.search(&SearchRequest::new("helo"))?;
    assert!(!page.hits.is_empty());
    println!("{:?} {:?}", page.relaxation.level, page.relaxation.corrected());
    // Fuzzy [Correction { original: "helo", corrected: "hello" }]

    db.close()?;
    Ok(())
}
```

`Db::open` creates the file and any missing parent directories, and opens an existing database without replacing it (SPEC §8.1). No daemon and no configuration file are involved. With a fixed path, as in the README, a second run fails at `create_collection_json` with `ErrorCode::AlreadyExists`: use `db.collection("notes")` on an existing database, or delete `app.persia` first.

`OpenOptions` sets how a database opens: `read_only`, `create_if_missing`, `durability` (`Fsync` by default), `memtable_bytes`, `block_cache_bytes`, and more. `Db::open` accepts a path or a `file://` URL. `s3://`, `gs://`, and `az://` return `ErrorCode::Unsupported` in this build: object storage is planned (M12).

Every call returns `persia::Result`. Its `Error` carries a stable `ErrorCode` (`NotFound`, `AlreadyExists`, `Conflict`, `InvalidArgument`, `Corrupt`, `Unsupported`, `ReadOnly`, and others). The API reference is `cargo doc -p persia --open` in a clone of the repository.

## Collections and schemas

`Db::create_collection` takes a `CollectionSchema`, built with `SchemaBuilder`. `Db::create_collection_json` takes the same schema as JSON (SPEC §6.2):

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

Field types are `text`, `keyword`, `i64`, `f64`, `bool`, `datetime`, and `stored` (SPEC §6.1). A `text` field's `analyzer` is a built-in pipeline: `en` stems and drops stopwords. The names and what each one emits are [Text analysis](/guide/text-analysis/). Every document has a string `id` (at most 512 bytes, unique in the collection). `db.collection(name)` returns a handle on an existing collection, `collection_names` lists them, and `drop_collection` removes one.

## Documents and commits

A `Collection` takes documents as a `persia::Document` (`put`, `put_batch`), as JSON (`put_json`), or as any `serde` type (`put_struct`). It reads them back with `get`, `get_json`, and `get_struct`, and removes one with `delete`. Documents are stored as CBOR (SPEC §6.4).

`put` is an upsert by id, and `delete` writes a tombstone. A write that returned is visible to every later read on that database, committed or not (read-your-writes, SPEC §8.4). `commit` flushes every collection's writes into segments and publishes them. The database also commits in the background on its own thread, and at `Db::close`, which joins that thread.

A database has one writer at a time, across processes: a second read-write open fails at once with `ErrorCode::Conflict`. Other handles open it read-only with `OpenOptions::new().read_only(true)` and see new commits after `Db::refresh`. A `Db` is `Send + Sync`: share it by reference or in an `Arc`.

Large payloads go into the blob pack beside the file (`<name>.persia.blobs`): `Db::put_blob`, `Db::get_blob`, `Db::get_blob_range`, and `Collection::put_with_blob`. [Blob storage](/guide/blobs/) is that API and its write order. With the cargo feature `async`, `AsyncDb` and `AsyncCollection` return futures that need no runtime.

## Search

`Collection::search` takes a `SearchRequest`: `SearchRequest::new(q)` and the builders `fields`, `filter`, `soft_filter`, `sort`, `limit`, `offset`, `after` (a cursor), `facets`, `explain`, `relax`, and `proximity`. `Collection::search_json` takes the same request as JSON (SPEC §7.1):

```rust
use persia::Db;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let db = Db::open(dir.path().join("app.persia"))?;
    let notes = db.create_collection_json(
        r#"{"name": "notes", "fields": {"body": {"type": "text"}}}"#,
    )?;
    notes.put_json(r#"{"id": "n1", "body": "Hello, world"}"#)?;

    // Relaxation off: nothing is spelled "helo", so the page is empty.
    let page = notes.search_json(r#"{"q": "helo", "limit": 5, "relax": false}"#)?;
    assert!(page.hits.is_empty());

    // Relaxation on (the default): the typo still finds n1.
    let page = notes.search_json(r#"{"q": "helo", "limit": 5}"#)?;
    let ids: Vec<_> = page.hits.iter().map(|h| h.document.id()).collect();
    assert_eq!(ids, ["n1"]);

    db.close()?;
    Ok(())
}
```

A `SearchResponse` has `hits` (each with `document`, `score`, the relaxation `level` that found it, and a `cursor`), `facets`, `relaxation`, and `next_cursor()`. Scoring is BM25F plus an optional static rank (SPEC §7.2). %%MATCHING_CELL%% How a hit is scored is [BM25F ranking](/guide/ranking/). Facets, sort, cursors, deadlines, and explain are [Faceted search](/guide/faceted-search/).

### Relaxation

%%RELAXATION_NOW%% If a query has fewer than `min_results` hits (default 1), the engine walks a cascade and stops at the first level that has enough hits (SPEC §7.6). The cascade is [Typo-tolerant search](/guide/typo-tolerant-search/). Edit distance, the correction, and the fallback are [Fuzzy search](/guide/fuzzy-search/). Synonym groups and prefix expansion are [Synonym and prefix search](/guide/synonym-and-prefix-search/).

| Level | Name | What changes |
|---|---|---|
| L0 | `exact` | Every analyzed term is required |
| L1 | `synonyms` | Collection synonyms, OR within a group |
| L2 | `drop_terms` | Require fewer terms. A term no document holds is least important, then a stopword, then a lower IDF |
| L3 | `prefix` | Prefix-expand the last term, and other analyzed terms of length at least 3 |
| L4 | `fuzzy` | Edit distance 1 (length 4–7) or 2 (length 8 or more). Report a correction when this step found the hits |
| L5 | `fallback` | Static rank, then index-location recency, flagged with `fallback`. `none` builds no step |
| L6 | `filters` | Only with `relax.filters: "last_resort"`, after every enabled earlier level: L0's words without the request's `soft_filter` (the hard `filter` stays). A query that searches no word does not build it. `"filters_dropped": true` only when this step found the hits |

Filters are not relaxed unless the caller sets `relax.filters` to `"last_resort"`. `relax: false` on a request turns the cascade off.

The response says what happened. In Rust, `SearchResponse::relaxation` has `level`, `applied`, `original`, `effective`, and `truncated`, and the accessors `corrected()`, `fallback()`, and `filters_dropped()`. In the response JSON (SPEC §7.5.2) the same object reads, for the query `helo` above:

```json
{
  "level": "fuzzy",
  "applied": ["prefix", "fuzzy"],
  "original": "helo",
  "effective": "hello",
  "corrected": { "helo": "hello" },
  "truncated": false,
  "filters_dropped": false
}
```

## Command line

The binary is `persia`, in the `persia-cli` crate. From a clone of the repository, run it as `cargo run -p persia-cli -- <command>`. Import a JSON Lines file into a new database, search it, and print its statistics:

```bash
echo '{"name": "notes", "fields": {"body": {"type": "text"}}}' > notes.schema.json
echo '{"id": "n1", "body": "hello, world"}' > notes.jsonl
persia import app.persia --collection notes --schema notes.schema.json --input notes.jsonl
persia search app.persia --collection notes --query helo
persia stats app.persia
```

| Command | What it does |
|---|---|
| `persia import <file> --collection <name> [--schema <json>] [--input <jsonl>] [--json]` | Puts one document per JSON Lines line (standard input by default) and commits at the end. `--schema` creates the database and the collection if they are missing. The first bad line stops the import. |
| `persia search <file> --collection <name> (--query <text> \| --request <json>) [--json]` | Runs one search, read-only. `--query` uses every default, relaxation included. `--request` takes a request JSON. `--json` prints the response JSON. |
| `persia stats <file> [--json]` | What a read-only open reports: the last commit, live and dead space, the WAL, the blob pack, and each collection's schema, documents, and segments. |
| `persia dump documents <file> --collection <name>` | Writes a collection's live documents as JSON Lines, the form `import` reads. |
| `persia verify <file> [--quick \| --deep]` | Checks a whole database read-only, including its blob pack. |
| `persia compact <file> [--json]` | Rewrites the container without its dead frames. |
| `persia gc <file> [--grace <age>] [--json]` | Collects the blob pack: deletes the blobs no document references. |
| `persia bench [--docs <n>] [--queries <n>] [--seed <n>] [--json]` | A quick micro-benchmark of this build in a scratch directory. Its numbers are local guidance only, not a published benchmark. |
| `persia inspect container <file> [--id <id>]` | The superblock and the frames after it. |
| `persia inspect segment <file> --schema <json> [--verify]`, `persia dump segment <file> --schema <json> --field <name> [--term <term>]` | One segment file: its structure, or a field's terms and postings. |
| `persia container files <file>`, `persia container sweep <file> [--quarantine]` | Lists or removes the temporary and quarantine files next to a database. |
| `persia store files <root>`, `persia store sweep <root> [--force]` | The same for a local object store directory. |

`persia --help` and `persia <command> --help` print every option. Exit status is 0 on success, 2 on a usage error, 3 on corrupt data or a missing referenced blob, 4 when the file is not a Persia container, and 1 on any other failure.

## What is not there yet

- **Server.** `persia-server` is a placeholder binary: it prints "not implemented (M11)" and exits with status 1. No gRPC or HTTP server runs.
- **Object storage.** S3, GCS, and Azure Blob backends are M12. Kubernetes sharding and stateless query nodes are M13.
- **SDKs.** Rust and Go network clients and a TypeScript network client follow the server. Embedded Go, TypeScript, and Python bindings that open a file in-process are planned for v1.0 ([Embedded SDKs](/guide/sdks/)).
- **A release.** Nothing is on crates.io, and the on-disk format can change before v1.0.

## The crates underneath

`persia` is the stable surface. The engine crates below it are in the same repository and are unpublished the same way; they are not stable APIs. `persia-format` holds the on-disk codecs, `persia-storage` the `Storage` trait and the single-file container, `persia-analysis` the tokenizers and analyzers, `persia-engine` segments, the WAL, query execution, and relaxation, and `persia-blob` the blob pack. [Crate architecture](/guide/architecture/) shows the layers, and the [code](/guide/code/) page quotes their exports.

## Common questions

The [FAQ](/guide/faq/) answers the same kind of question for status, how this differs from SQLite FTS5, Tantivy, and Meilisearch, the file format, crash safety, SDKs, and the license.

### Can I add Persia to a Rust project today?

Yes, as a Git dependency, with the pre-alpha caveats above: no release, and a format that can still change. See [Add Persia to a Rust project](#add-persia-to-a-rust-project).

### How do I read which relaxation was applied?

From `SearchResponse::relaxation` (SPEC §7.6): `level`, `applied`, `original`, `effective`, `corrected()`, `fallback()` when L5 found the hits, and `filters_dropped()`. Each hit also carries the `level` that found it, and stricter levels rank above looser ones. See [Typo-tolerant search](/guide/typo-tolerant-search/) and [Fuzzy search](/guide/fuzzy-search/).

### Is the on-disk format stable?

No. Format version 1.0 is a draft until the v1.0 release (SPEC §15). There is no compatibility promise. A change to the bytes requires a SPEC edit, a version or feature-flag bump, an ADR, and a golden fixture.

### Is there a server, a cloud backend, or a client SDK?

Not yet. gRPC and HTTP are M11. S3, GCS, and Azure Blob are M12. Kubernetes sharding is M13. Rust and Go network clients are 14.2 and 14.3, compatibility is 14.4, and the TypeScript network client is 14.5, after M11 and M13. Embedded Go, TypeScript, and Python bindings are scheduled for v1.0 as ROADMAP 14.6 ([Embedded SDKs](/guide/sdks/)). Embedded mode has no authentication and no config file (SPEC §14).

### Why is a query supposed to avoid empty results?

That is a product rule (SPEC §7.6), not an optional plugin. A query that matches nothing is relaxed step by step, and the response says what changed. %%RELAXATION_CELL%% [Typo-tolerant search](/guide/typo-tolerant-search/) is the cascade. [Fuzzy search](/guide/fuzzy-search/) is the edit-distance step. [Faceted search](/guide/faceted-search/) is the request that search runs.

### Where is the source of truth?

[SPEC.md](https://github.com/tomiok/persiaDB/blob/main/SPEC.md) for behavior. [ROADMAP.md](https://github.com/tomiok/persiaDB/blob/main/ROADMAP.md) for planned work. [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md) for what is done. This site is a reading of those files. [For AI agents](/guide/for-agents/) is the short version of the same constraints.
