---
title: "Embedded API"
description: "The persia crate's embedded Rust API: Db::open, OpenOptions, collections, documents, search, blobs, errors, and the async handles."
head:
  - tag: title
    content: "Embedded Rust API: Db::open, collections, and search | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Embedded Rust API: Db::open, collections, and search | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Embedded Rust API: Db::open, collections, and search | PersiaDB"
---

The `persia` crate is the embedded API: open a file with `Db::open`, and it works, with no daemon and no configuration file (SPEC §8, §9.6, §14). The usage walkthrough, including the quickstart and the FAQ, is [Getting started](/guide/getting-started/). %%M10_BLOCKED%% The crates are unpublished (`publish = false`): depend on the Git repository. The API reference is `cargo doc -p persia --open` in a clone.

## A whole program

The sample opens a database in a temporary directory (the `tempfile` crate); any path works.

```rust
use persia::{
    Db, Document, ErrorCode, FieldKind, OpenOptions, SchemaBuilder, SearchRequest, TextOptions, Value,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("app.persia");
    let db = Db::open(&path)?; // created: it did not exist

    // A schema built in code; `create_collection_json` takes the same schema as JSON (SPEC §6.2).
    let schema = SchemaBuilder::new("notes")
        .field("body", FieldKind::Text(TextOptions::default()))
        .build()?;
    let notes = db.create_collection(schema)?;
    let body = notes.schema()?.field("body").map(|f| f.id()).ok_or("no body field")?;

    // A write is visible to every later read, before any commit (read-your-writes, SPEC §8.4).
    notes.put(&Document::new("n1").with(body, Value::String("hello world".into())))?;
    assert!(notes.get("n1")?.is_some());
    let page = notes.search(&SearchRequest::new("hello").limit(10))?;
    assert_eq!(page.hits.len(), 1);

    // One writer per file: a second read-write open fails at once.
    let second = Db::open(&path).err().ok_or("a second writer opened")?;
    assert_eq!(second.code(), ErrorCode::Conflict);

    // A reader beside the writer shows what its open found, and the writer's later writes after `refresh`.
    let reader = OpenOptions::new().read_only(true).open(&path)?;
    notes.put(&Document::new("n2").with(body, Value::String("hello again".into())))?;
    assert!(reader.collection("notes")?.get("n2")?.is_none());
    reader.refresh()?;
    assert!(reader.collection("notes")?.get("n2")?.is_some());

    reader.close()?;
    db.close()?; // the final commit, then the writer lock is free
    Ok(())
}
```

## Open and close

- `Db::open` takes a path or a `file://` URL. It creates the container and any missing parent directories when nothing is at the path, and opens an existing database without replacing it (SPEC §8.1). `s3://`, `gs://`, and `az://` return `ErrorCode::Unsupported` in this build: object storage is milestone M12.
- `OpenOptions::new()` (also its `Default`) sets how a database opens, and `OpenOptions::open` opens it: `create_if_missing` (on by default; `false` requires the file to exist and creates nothing), `read_only` (opens an existing file, creates nothing, takes no lock), `durability`, `memtable_bytes`, `idle_flush`, `commit_interval`, `merge_threads`, `block_cache_bytes`, `mapped_reads`, and with the `async` feature `async_threads`. Read-only wins when creation is also requested, including the default: that combination is not an error, nothing is created, and a missing path fails. Go, TypeScript, and the C ABI are specified to use that same rule (SPEC §13.1).
- A read-write `Db` commits and merges on its own maintenance thread, as its `OpenOptions` say. A program never schedules a commit or a merge itself (SPEC §8.2, ROADMAP 10.2.6). `Db::close` makes the final commit and joins that thread. Dropping a `Db` waits at most 5 seconds and cannot report an error, so prefer `close`.
- A read-only `Db` shows one fixed state until `Db::refresh`. `Db::backup` writes a copy of the database, and of its blob pack, to a new path (SPEC §4.8). `Db::path`, `Db::is_read_only`, and `Db::block_cache_stats` read the handle.

## Collections and documents

- `Db::create_collection` (a `CollectionSchema` from `SchemaBuilder`), `Db::create_collection_json`, `Db::collection`, `Db::collection_names`, and `Db::drop_collection`. `Collection::schema` reads the schema back. Creating and dropping a collection are committed before the call returns.
- `Collection::put` (a `Document`), `put_batch`, `put_json`, `put_struct` (any `serde::Serialize`), `get`, `get_json`, `get_struct`, `to_struct`, and `delete`. `put` is an upsert by id. A `put_batch` that returns is seen whole from then on, and none of it before.
- `Collection::commit` (or `Db::commit`) flushes every collection's writes into segments. A write is durable at its `Durability` whether or not it is committed. `Collection::with_durability` gives a handle its own level. [Durability and crash recovery](/guide/durability/) is what each level keeps.

## Search

`Collection::search` takes a `SearchRequest` (`SearchRequest::new(q)` and its builders), and `Collection::search_json` takes the request JSON (SPEC §7.1). A `SearchResponse` holds `hits`, `facets`, `relaxation`, and `query_truncated`, and `SearchResponse::to_json` writes the response JSON (SPEC §7.5.2). %%QUERY_CELL%%

When a query finds fewer than `min_results` hits, the search walks the relaxation cascade (SPEC §7.6). L4 fuzzy rewrites a typo and reports the correction when that step found the hits; L5 fallback ranks by static rank, then index-location recency, ignores the request's `sort`, and flags that order (`Relaxation::fallback`). With `relax.filters: "last_resort"`, one last step (L6) drops the request's `soft_filter` (the hard `filter` stays) when every earlier level found fewer than `min_results` hits. A query that searches no word does not plan it, and `Relaxation::filters_dropped` is true only when that step supplies the hits (ROADMAP 8.2.6). %%RELAXATION_CELL%% See [Synonym and prefix search](/guide/synonym-and-prefix-search/), [Typo-tolerant search](/guide/typo-tolerant-search/), [Filtered search](/guide/filtered-search/), and [Faceted search](/guide/faceted-search/).

## Blobs

`Db::put_blob` stores bytes from any `std::io::Read` and returns a `BlobRef` once the blob is durable. `Db::get_blob` returns a `BlobReader`, and `Db::get_blob_range` reads one byte range. `Collection::put_with_blob` stores the blob first, then logs the document that references it. [Blob storage](/guide/blobs/) is that API and its write order.

## Errors

Every call returns `persia::Result`. Its `Error` is opaque: `Error::code` returns a stable `ErrorCode` (`NotFound`, `AlreadyExists`, `Conflict`, `InvalidArgument`, `Corrupt`, `Unsupported`, `Io`, `Transient`, `Closed`, `Limit`, `ReadOnly`, `Poisoned`, `Cancelled`, and `Busy` and `Panic`, which only the C ABI returns), `Error::message` the text, and `std::error::Error::source` the lower layer's error. Corrupted bytes are `ErrorCode::Corrupt`, never a panic. The codes are the numbers of the [C ABI's table](https://github.com/tomiok/persiaDB/blob/main/docs/sdk/c-abi.md) (`ErrorCode::number`, `ErrorCode::as_str`).

## Threads and async

`Db` is `Send` and `Sync`: share it by reference or in an `Arc`. A `Collection` owns a share of the database and is `Clone`. Reads take no lock and never wait for a write. A second writer fails immediately, in this process or another (SPEC §8.4).

With the cargo feature `async`, `AsyncDb` and `AsyncCollection` offer the same calls as futures (`JobFuture`) that run on a small pool of the database's own threads (`OpenOptions::async_threads`, `min(4, cores)` by default). They need no async runtime. The synchronous API is the core (ADR-0064).

## Configuration

Embedded mode has no authentication and no config file. Knobs are `OpenOptions` builders with defaults (SPEC §14): a 64 MiB memtable with a 10 second idle flush, a 60 second commit interval, `Fsync` durability, one merge thread, and an 8 MiB block cache. Blob options are per call (`PutBlobOptions`), and relaxation settings are per collection (the schema's `relaxation`) or per request (`SearchRequest::relax`).

## Command line

The `persia` binary (`persia-cli`) works on the same files: `stats`, `verify`, `compact`, `gc`, `import`, `search`, `dump documents`, `bench`, and the `inspect` and file-sweep commands. [Getting started](/guide/getting-started/#command-line) lists them. The `v0.1.0-alpha` tag is specified to say, in the release itself, that the format is still unstable (ROADMAP 10.5.2).
