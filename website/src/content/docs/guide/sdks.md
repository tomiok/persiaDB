---
title: "Embedded SDKs"
description: "The planned in-process Go, TypeScript, and Python bindings. Not implemented. Network clients are a different milestone."
head:
  - tag: title
    content: "Planned embedded SDKs for Go, TypeScript, and Python | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Planned embedded SDKs for Go, TypeScript, and Python | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Planned embedded SDKs for Go, TypeScript, and Python | PersiaDB"
---

**Planned. Not implemented.** There is no Go module, no npm package, and no Python package. The Rust API these bindings
will wrap is the [`persia`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs) crate (`Db`, `Collection`,
`Collection::search`). Nothing on this page opens a database from Go, TypeScript, or Python.

This page is the short version. The design is
[ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md)
and [`docs/sdk/`](https://github.com/tomiok/persiaDB/blob/main/docs/sdk/README.md).
When this page and the SPEC disagree, the SPEC wins.

## Two different SDKs

SPEC §13 names both, and they are easy to mix up.

| | Embedded bindings | Network clients |
|---|---|---|
| Who calls them | A Go, TypeScript, or Python program that wants a file, the way Rust calls `Db::open` | A program talking to `persia-server` |
| Paths | `sdk/persia`, `sdk/node`, `sdk/python` (none of these directories exist) | `sdk/rust` (`persia-client`), `sdk/go` (`persia-go`) |
| Milestone | 14.6 (v1.0), over the `persia` facade (M10). Design: ADR-0018. The same leaves stay listed under M16 and are checked when 14.6 is done. 16.4.1 and 16.6 stay v1.1. | Rust and Go are 14.2 and 14.3, compatibility is 14.4, and the TypeScript network client is 14.5, after the server (M11) and sharding (M13) |
| Status | Design only | Not started |

The network clients are described on [Server, cloud, and Kubernetes](/guide/server/).

## Recommendation

One engine, the Rust one (ADR-0001). Go, TypeScript, and Python do not grow a second search implementation.

- **Go** links a static library through cgo by default. An optional `persia_purego` build tag dynamically
  loads the shared library with `CGO_ENABLED=0` (Unix `Dlopen`, Windows `LoadLibrary`), behind the same API.
  The build does not fetch that library. The caller places it beside the binary, or sets `PERSIA_LIB` (read
  by `Open`). That binding is `persia-ffi`, a C ABI crate
  that does not exist yet. A pure-Go engine, and a WASM engine hosted by wazero, were rejected: durability
  is fsync and a writer lock (SPEC §4.5, §8.4), and mapped reads are already part of storage (ADR-0017).
- **TypeScript** ships first as an N-API addon (`@persiadb/node`) for Node.js, Bun, and Deno. The addon
  calls the same Rust functions the C ABI calls. A WASM package for browsers and edge isolates comes
  later, only after a storage backend exists that does not need mmap or POSIX fsync. That backend is
  ROADMAP M28 (target v2.3).
- **Python** binds the committed `persia.h` with cffi or ctypes (`sdk/python`, ROADMAP 14.6.8). Installing the package does not need a Rust toolchain. Wheels that ship the prebuilt library are 14.6.10. SPEC §13 names the package in 14.6.11. There is no separate Python design page; the contract is the C ABI. The Python HTTP client (21.2) is a different library and stays v1.2.

Prebuilt binaries are how a caller builds without Rust: a checksummed `libpersia.a` for Go,
per-platform optional npm dependencies for Node, and Python wheels. Until format v1.0 there is no compatibility promise
(SPEC §15). The libraries are 14.6 (v1.0). 16.4.1 publishes them only after the v1.0 tag (M15). ROADMAP 10.5.2's
`v0.1.0-alpha` tag is the core's notice that the format is still unstable; it is not the binding cut.

## The call, once it exists

Illustrative. These are not functions in a repository.

```go
db, err := persia.Open(ctx, "app.persia")
products, err := db.CreateCollection(ctx, schema)
err = products.Put(ctx, doc)
err = products.Commit(ctx)
results, err := products.Search(ctx, persia.Text("runing"))
```

```ts
const db = await open("app.persia");
const products = await db.createCollection(schema);
await products.put(doc);
await products.commit();
const results = await products.search("runing");
```

`Open` and `open` create the container and any missing parent directories. `WithCreate(false)` and
`{ create: false }` require the file to exist. Read-only does not create it, and it wins when creation
is also requested (SPEC §8.1).

`Put` of an existing id is the update (SPEC §8.3). There is no `BEGIN` / `ROLLBACK`: multi-document
transactions are out of scope (SPEC §1.2). A put is specified to be visible to a later search on that
database before commit (SPEC §8.2). Commit is the durable manifest. A second writer fails immediately
(SPEC §8.4).

Search is BM25F (SPEC §7.2). A query that does not match is specified to relax — synonyms, dropped terms, prefix,
edit distance, then a fallback — and the response says what changed (SPEC §7.6). %%RELAXATION_CELL%%
[Synonym and prefix search](/guide/synonym-and-prefix-search/) is the L1 and L3 steps.
The `persia` crate already runs that cascade (`Collection::search`), so the bindings wrap it and do
not implement it themselves. Empty hits are still a successful response when relaxation is
off or the budget runs out. Facets, sort, and cursors on the engine are [Faceted search](/guide/faceted-search/).

Go methods take `context.Context` and return `error` values with `errors.Is` sentinels. TypeScript
methods return promises and throw `PersiaError` with a `code`; `get` of a missing id returns `null`.
The engine reads a search deadline and a cancellation token (SPEC §7.3). The `persia` facade takes the deadline as
`SearchRequest::timeout` (`"timeout_ms"` in the request JSON), and an async search (`AsyncCollection::search`)
dropped before it ends is cancelled. These planned bindings do not wire `context.Context` or `AbortSignal` to
them yet. The cascade budget is `relax.budget_ms` on the query,
separate from the search deadline. [Faceted search](/guide/faceted-search/) is that split.

Pagination is a page per call (`limit` / `offset`, or a `search_after` cursor, SPEC §7.4). There is no
iterator that borrows the index across the language boundary.

Documents cross the boundary as UTF-8 JSON in the shapes SPEC §6.2 and §7.1 sketch. Go structs and
TypeScript objects are encoded by the language package. The core stores CBOR (SPEC §6.4).

A fault on a mapped page kills the process (ADR-0010). Mapped reads are opt-in and off by default
(ADR-0017). A Rust panic is caught at the boundary and becomes an error; the handle is then poisoned.

## What has to exist first

The bindings wrap the Rust API (ROADMAP 10.1): `Db::open`, collections, put, delete, get,
commit, search, and a stable error type (`ErrorCode`, whose numbers are the C ABI's codes). %%M10_BLOCKED%% Blobs in the SDK wait on the same work, plus the upload-staging
decision (SPEC §18.8). Opening `s3://` waits on cloud backends (M12). The WASM package waits on a
storage backend nobody has designed.

Live status is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md).
The phased table is in the [design index](https://github.com/tomiok/persiaDB/blob/main/docs/sdk/README.md).
