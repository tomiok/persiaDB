---
title: "Embedded SDKs"
description: "The planned in-process Go and TypeScript bindings. Not implemented. Network clients are a different milestone."
---

**Planned. Not implemented.** There is no Go module and no npm package. The Rust API these bindings
will wrap does not exist either: [`persia`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs#L1-L4)
exports no types. Nothing on this page opens a database.

This page is the short version. The design is
[ADR-0018](https://github.com/tomiok/persiaDB/blob/main/docs/adr/0018-embedded-sdk-bindings.md)
and [`docs/sdk/`](https://github.com/tomiok/persiaDB/blob/main/docs/sdk/README.md).
When this page and the SPEC disagree, the SPEC wins.

## Two different SDKs

SPEC §13 names both, and they are easy to mix up.

| | Embedded bindings | Network clients |
|---|---|---|
| Who calls them | A Go or TypeScript program that wants a file, the way Rust will call `Db::open` | A program talking to `persia-server` |
| Paths | `sdk/persia`, `sdk/node` (neither directory exists) | `sdk/rust` (`persia-client`), `sdk/go` (`persia-go`) |
| Milestone | M16 (target v1.1), after v1.0. Design: ADR-0018. Blocked on M15. | M14, after the server (M11) and sharding (M13) |
| Status | Design only | Not started |

The network clients are described on [Server, cloud, and Kubernetes](/guide/server/).

## Recommendation

One engine, the Rust one (ADR-0001). Go and TypeScript do not grow a second search implementation.

- **Go** links a static library through cgo. The library is `persia-ffi`, a C ABI crate that does not
  exist yet. A pure-Go engine, and a WASM engine hosted by wazero, were rejected: durability is fsync
  and a writer lock (SPEC §4.5, §8.4), and mapped reads are already part of storage (ADR-0017).
- **TypeScript** ships first as an N-API addon (`@persiadb/node`) for Node.js, Bun, and Deno. The addon
  calls the same Rust functions the C ABI calls. A WASM package for browsers and edge isolates comes
  later, only after a storage backend exists that does not need mmap or POSIX fsync. That backend is
  ROADMAP M28 (target v2.3).

Prebuilt binaries are how a caller builds without Rust: a checksummed `libpersia.a` for Go, and
per-platform optional npm dependencies for Node. Until format v1.0 there is no compatibility promise
(SPEC §15). The first binding release follows a core tag that says the format is unstable
(ROADMAP 10.5.2).

## The call, once it exists

Illustrative. These are not functions in a repository.

```go
db, err := persia.Open(ctx, "app.persia", persia.WithCreate())
products, err := db.CreateCollection(ctx, schema)
err = products.Put(ctx, doc)
err = products.Commit(ctx)
results, err := products.Search(ctx, persia.Text("runing"))
```

```ts
const db = await open("app.persia", { create: true });
const products = await db.createCollection(schema);
await products.put(doc);
await products.commit();
const results = await products.search("runing");
```

`Put` of an existing id is the update (SPEC §8.3). There is no `BEGIN` / `ROLLBACK`: multi-document
transactions are out of scope (SPEC §1.2). A put is specified to be visible to a later search on that
database before commit (SPEC §8.2). Commit is the durable manifest. A second writer fails immediately
(SPEC §8.4).

Search is BM25F (SPEC §7.2). A query that does not match is relaxed — synonyms, dropped terms, prefix,
edit distance, then a fallback — and the response says what changed (SPEC §7.6). That cascade is M8.
M10 depends on M8, so the first core release that the bindings can wrap already includes it. The
bindings do not implement it themselves. Empty hits are still a successful response when relaxation is
off or the budget runs out.

Go methods take `context.Context` and return `error` values with `errors.Is` sentinels. TypeScript
methods return promises and throw `PersiaError` with a `code`; `get` of a missing id returns `null`.
Neither context nor `AbortSignal` can cancel a call that has already entered the engine. The engine is
synchronous, and it has no cancel flag. The query budget is `relax.budget_ms` on the query.

Pagination is a page per call (`limit` / `offset`, or a `search_after` cursor, SPEC §7.4). There is no
iterator that borrows the index across the language boundary.

Documents cross the boundary as UTF-8 JSON in the shapes SPEC §6.2 and §7.1 sketch. Go structs and
TypeScript objects are encoded by the language package. The core stores CBOR (SPEC §6.4).

A fault on a mapped page kills the process (ADR-0010). Mapped reads are opt-in and off by default
(ADR-0017). A Rust panic is caught at the boundary and becomes an error; the handle is then poisoned.

## What has to exist first

The bindings are blocked on the Rust API (ROADMAP 10.1): `Db::open`, collections, put, delete, get,
commit, search, a stable error type. That milestone is blocked on the write path (M6), query (M7),
relaxation (M8), and blobs (M9). Blobs in the SDK wait on the same work, plus the upload-staging
decision (SPEC §18.8). Opening `s3://` waits on cloud backends (M12). The WASM package waits on a
storage backend nobody has designed.

Live status is [PROGRESS.md](https://github.com/tomiok/persiaDB/blob/main/PROGRESS.md).
The phased table is in the [design index](https://github.com/tomiok/persiaDB/blob/main/docs/sdk/README.md).
