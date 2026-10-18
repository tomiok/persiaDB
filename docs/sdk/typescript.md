# TypeScript SDK (embedded)

**Design only.** No npm package, no addon, and no function you can import. The snippets below are the
API [ADR-0018](../adr/0018-embedded-sdk-bindings.md) reserves. They follow SPEC §6.2, §7.1, §7.6, §8.1,
and §9.6, and the Rust facade those sections will become. That facade is a stub:
[`crates/persia/src/lib.rs`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs#L1-L4).

The first package is a native addon for Node.js, Bun, and Deno. A WASM build for the browser and for
edge isolates is a later package, specified at the bottom, and it is not started with the addon.

## Packages

| Package | Directory | Runtime | When |
|---|---|---|---|
| `@persiadb/node` | `sdk/node` (outside the Cargo workspace) | Node.js 20 and 22, Bun, Deno | First release, after ROADMAP M10 |
| `@persiadb/node-<platform>` | built by CI, not hand-written | one OS/arch each | Published as optional dependencies of `@persiadb/node` |
| `@persiadb/wasm` | `sdk/wasm` | browsers, edge isolates | After a `Storage` backend that does not need mmap or POSIX fsync. No milestone for that backend today. |

`sdk/node` is a napi-rs crate that depends on the `persia-ffi` rlib by path. It is not a Cargo workspace
member, so its `build.rs` does not violate ADR-0003, which covers the workspace. It calls the safe Rust
API inside `persia-ffi`, the same functions the C exports call. It does not `dlopen` `libpersia.so`.
Error codes and JSON documents are the ones in [c-abi.md](./c-abi.md), so the addon and the Go module
cannot drift.

The npm scope is a proposal. Confirm it is available before the first publish (open question in the
[index](./README.md)).

## What a caller writes

```ts
import { open } from "@persiadb/node";

const db = await open("app.persia", { create: true });
try {
  const products = await db.createCollection({
    name: "products",
    fields: {
      title: { type: "text", analyzer: "en", boost: 3 },
      body: { type: "text", analyzer: "en" },
      brand: { type: "keyword", facet: true },
      price: { type: "f64" },
      sales: { type: "i64" },
    },
    relaxation: { enabled: true, minResults: 1 },
  });

  // Upsert by id (SPEC §8.3). Putting the same id again is the update.
  await products.put({
    id: "sku-1",
    title: "Running shoes",
    brand: "nike",
    price: 80,
    sales: 12,
  });
  await products.commit();

  // The typo is the query. Relaxation is on by default (SPEC §7.6).
  const results = await products.search("runing");
  for (const hit of results.hits) {
    console.log(hit.id, hit.score);
  }
  if (results.relaxation) {
    console.log(results.relaxation.level, results.relaxation.corrected);
  }
} finally {
  await db.close();
}
```

Collection handles are closed with the database when `db.close()` runs, if the caller has not closed
them already. Closing a database that still has a blob reader open fails with `PersiaError` code
`"busy"`, matching the C ABI. `Explicit Resource Management` (`await using`) is supported on the
classes once the package ships, as a convenience over `close()`, not as a second lifetime.

## API

Every method that does work returns a `Promise`. The native call runs on the N-API async thread pool
(`napi::AsyncTask` or the equivalent napi-rs async work), so a search does not block the JavaScript
event loop. `close()` is also async because it waits for in-flight native calls and for the core's
background threads (ROADMAP 10.2.3).

```ts
export function open(path: string, options?: OpenOptions): Promise<Database>;

export interface CallOptions {
  signal?: AbortSignal;
}

export interface OpenOptions extends CallOptions {
  create?: boolean;
  readOnly?: boolean;
  // Further fields appear only when Rust OpenOptions has the SPEC §14 knob.
}

export class Database {
  createCollection(schema: CollectionSchema, options?: CallOptions): Promise<Collection>;
  collection(name: string, options?: CallOptions): Promise<Collection>;
  dropCollection(name: string, options?: CallOptions): Promise<void>;
  close(): Promise<void>;
}

export class Collection {
  put(doc: DocumentInput, options?: CallOptions): Promise<void>;
  putBatch(docs: readonly DocumentInput[], options?: CallOptions): Promise<void>;
  delete(id: string, options?: CallOptions): Promise<void>;
  get<T extends Document = Document>(id: string, options?: CallOptions): Promise<T | null>;
  commit(options?: CallOptions): Promise<void>;
  search(query: string | SearchRequest, options?: CallOptions): Promise<SearchResponse>;
  schema(): Promise<CollectionSchema>;
  close(): Promise<void>;
}
```

A synchronous twin, `@persiadb/node/sync`, is reserved for one-shot scripts and is not the default
export. Its methods block the event loop on purpose and are named `openSync`, `putSync`, `searchSync`.
Servers use the promises. The sync entry point is the same rlib, called on the JavaScript thread.

### Documents and types

```ts
export interface Document {
  id: string;
  [field: string]: unknown;
}

export type DocumentInput = { id: string } & Record<string, unknown>;
```

`put` accepts a plain object. The addon JSON-encodes it (the same UTF-8 the C ABI takes) and the core
stores CBOR (SPEC §6.4). Class instances are not a supported input: methods and prototypes do not
survive `JSON.stringify`, and the package rejects a value whose `toJSON` would hide `id`.

TypeScript generics stop at the boundary. `get<Product>` checks nothing at runtime beyond the JSON the
core returned. The schema is the authority (SPEC §6.2). A future codegen that emits an interface from a
schema is out of the first release; hand-written interfaces are enough, and a generator that drifted
from the core would be worse than `unknown` fields.

Field types are the SPEC §6.1 set: `text`, `keyword`, `i64`, `f64`, `bool`, `datetime`, `stored`. An
`i64` field rejects a JSON number that is not an integer (`"invalid"`). `datetime`'s JSON spelling is
open until M3 defines it (index, question 4). The TypeScript type for that field will change to match
M3 in the same release that implements it, which is allowed while the package is 0.x.

`put` of an existing id is the update. There is no `update` method.

`putBatch` sends one JSON array and one native call. Atomicity is whatever ROADMAP 10.1.3 specifies.
SPEC §8.1 names the operation and does not say.

`get` returns `null` when the id is absent. That is the normal outcome, and throwing would make every
lookup a `try`. Corruption, I/O, and a poisoned handle still throw. Go uses a sentinel instead
([go.md](./go.md)); both map to `PERSIA_NOT_FOUND`. If the Rust API picks a different split, both
bindings follow it.

Unknown document fields are rejected when the collection is strict, the default (SPEC §6.4). The error
code is `"invalid"`.

### Search, ranking, relaxation

```ts
export interface SearchRequest {
  q?: string;
  fields?: string[];             // "title^3"; omitted means schema boosts
  filter?: Filter;
  sort?: "score" | SortField[];
  limit?: number;                // default 20
  offset?: number;
  searchAfter?: Cursor;
  facets?: string[];
  relax?: RelaxOptions | false;  // false turns the cascade off
  explain?: boolean;
  signal?: AbortSignal;
}

export interface SearchResponse {
  hits: Hit[];
  facets: Record<string, FacetBucket[]>;
  relaxation: Relaxation | null;
  manifestSeq: bigint;
}

export interface Hit {
  id: string;
  score: number;
  fields: Record<string, unknown>;
  blob: BlobRef | null;
  cursor: Cursor;
}

export interface Relaxation {
  level: "exact" | "synonyms" | "drop_terms" | "prefix" | "fuzzy" | "fallback";
  applied: string[];
  original: string;
  effective: string;
  corrected: Record<string, string>;
  truncated: boolean;
}
```

`search("runing")` is `search({ q: "runing" })`. Ranking is BM25F plus the collection's static rank
(SPEC §7.2), computed in the core. The addon does not rescore.

`relaxation` is `null` when the query matched at L0 (`exact`) and the cascade did not change the query.
When it is present, `level` is the level that supplied the hits, `corrected` maps the tokens the fuzzy
level rewrote, and `original` / `effective` are the query text before and after (SPEC §7.6). Results
from a stricter level rank above results from a looser one. `truncated` means the time budget
(`relax.budget_ms`, default 50 ms) stopped the cascade and the response is the best so far.

`search({ relax: false })` disables that. Filters stay strict unless `relax.filters` is
`"last_resort"`.

`explain: true` throws `"unsupported"` until SPEC §7.7 is implemented. The field exists so turning the
feature on later is not an API break inside 0.x... and 0.x may still break. The field is there so the
post-1.0 API does not need a new method name.

An empty `hits` array is a successful response, not an exception.

### Pagination

`search` returns one page. There is no object that streams hits off the native heap.

```ts
let after: Cursor | undefined;
for (;;) {
  const page = await products.search({ q: "shoes", limit: 20, searchAfter: after });
  if (page.hits.length === 0) break;
  for (const hit of page.hits) {
    // ...
  }
  after = page.hits[page.hits.length - 1].cursor;
}
```

`cursor` is the SPEC §7.4 triple `(score, segment_gen, docid)`. Each iteration is a new call and can
observe a commit that happened since the previous page. A cursor a merge has invalidated rejects with
the core's error. `offset` is for a shallow page, not for walking the collection.

`manifestSeq` is a `bigint` because a manifest sequence is a `u64` and `number` cannot hold every one.
Hit scores stay `number` (f64).

### Blobs

Blocked on M9 and on the upload-staging decision (SPEC §18.8). The methods, once the core has them:

```ts
const ref = await products.putBlob(bytes, { mime: "application/pdf" });
await products.put({ id: "sku-1", title: "Manual", blob: ref });
await products.commit();

const chunk = await products.getBlob(ref.id, { offset: 0n, length: 1024n * 1024n });
```

`bytes` is a `Uint8Array`. `getBlob` returns a `Uint8Array` for a range that fits a single buffer, and
`openBlob` returns an async iterator of `Uint8Array` chunks for a longer read. The iterator pulls into
a buffer the addon allocated for that call; it is not a JavaScript callback invoked from Rust. That is
the pull reader in the [C ABI](./c-abi.md).

`put` of the document happens after `putBlob` resolves. A helper `putWithBlob` runs those steps in the
SPEC §9.5 order. The id on `BlobRef` is 64 lowercase hex characters
([`BlobId`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/blob.rs#L19-L43)).
The application does not hash the file.

### Transactions and commits

No `begin` and no `rollback`. SPEC §1.2 excludes multi-document transactions.

`commit` publishes a manifest and, by default, fsyncs (SPEC §8.2, §4.5). `put` is visible to a later
`search` on that database before `commit` returns (read-your-writes). Durability across a crash waits
for `commit`. The package does not change that default and does not buffer writes in JavaScript.

`close` waits for in-flight searches, then asks the core to stop flush and merge threads and drop the
writer lock.

## Errors

Failures throw `PersiaError`, a subclass of `Error`.

```ts
export type PersiaErrorCode =
  | "not_found"
  | "already_exists"
  | "conflict"
  | "invalid"
  | "corrupt"
  | "unsupported"
  | "io"
  | "transient"
  | "busy"
  | "panic"
  | "closed"
  | "limit";

export class PersiaError extends Error {
  readonly code: PersiaErrorCode;
  readonly status: number; // the persia_status integer
}
```

`get` is the exception to throwing: absence is `null`. `search` that relaxes a typo is a normal
response. `code === "conflict"` is the second writer (SPEC §8.4) and it fails immediately.

`code === "panic"` means Rust unwound, the handle is poisoned, and `close()` is the only useful call.
The process stays up. A mapped-page fault does not throw: it kills the process (ADR-0010,
[mmap.rs L1–L8](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/mmap.rs#L1-L8)).
Mapped reads stay off unless the caller opts in, and only after that option has been tested under Node
on each OS. The default path is ordinary reads (ADR-0017).

The package does not retry `"transient"`. The core already retries the storage operations that have a
defined retry (SPEC §10.1).

Result types (`Result<T, PersiaError>` as a return value) were considered and rejected for this
package. The surrounding ecosystem uses exceptions (`fs/promises`, database clients). A `code` field
keeps matching exhaustive in the call sites that care. Go's sentinel errors are the other language's
idiom, over the same codes.

## Promises, AbortSignal, and workers

The native call is synchronous Rust on a libuv worker thread (or Bun/Deno's equivalent of the N-API
async pool). The JavaScript caller `await`s it. `OpenOptions` extends `CallOptions`, so `open` takes
the same `signal` as every other call.

`signal` is also a field of `CallOptions`, the optional last argument of `put`, `putBatch`, `delete`,
`get`, `commit`, and `search`, and a field of `SearchRequest`. Passing both is the same signal; if
they disagree, the call rejects with `"invalid"` before any native work.

The signal is observed before the native call is queued. If it has already aborted, the promise
rejects with `PersiaError` code `"invalid"` and `cause` set to the abort reason, and the native call
does not start. Once the call is running, abort does not preempt it. The same limit as Go's context:
the engine has no cancel flag
([storage.rs L12–L14](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/storage.rs#L12-L14)).
The promise resolves or rejects with the operation's outcome. A search's own time bound is
`relax.budget_ms`, not the abort signal.

When the Rust API can observe a cancel flag, the addon will pass one through and this section changes.

### Worker threads

A `Database` cannot be transferred with `worker_threads` or structured clone. The handle is a pointer
held by the addon. Each worker that needs the file calls `open` itself.

Several workers may open the same path read-only. One may open it read-write. A second read-write open,
in another worker or another process, rejects with `"conflict"` (SPEC §8.4). The addon does not install
a process-wide lock in JavaScript on top of the core's lock.

Closing in one worker does not affect another worker's handle. They are different opens.

Bun and Deno load N-API addons. The first release CI smoke-tests the three runtimes on the conformance
adapter (open, put, search, close) and treats a runtime that cannot load the addon as a documented gap,
not as a silent skip. If Deno's Node-API compatibility cannot host the addon on a given release, the
package readme says so and Deno waits. The API does not fork into a Deno-specific WASM build to paper
over that.

## WASM, later

`@persiadb/wasm` is the same TypeScript surface, with these differences written down now so the addon
API does not grow features WASM could never offer:

| Topic | Addon | WASM package |
|---|---|---|
| File open | A filesystem path. The core's container, fsync, and writer lock. | Not a POSIX path. The host supplies bytes or a storage interface the core does not have yet. |
| Durability | `commit` fsyncs (SPEC §4.5). | Whatever the future backend can promise. It must not claim the local commit protocol if it cannot fsync. |
| Mapped reads | Opt-in, process-killing fault on failure (ADR-0010). | Unavailable. |
| Threads | N-API async pool, many readers. | The host's WASM threads, if any. A single-threaded fallback must still honor the one-writer rule. |
| Panics | Caught, thrown as `PersiaError`. | Caught at the wasm-bindgen / napi boundary the package actually uses. An abort still kills the isolate. |
| Distribution | optionalDependencies, one `.node` per platform | One `.wasm` plus a JS loader. No node-gyp. |

The package is not a compilation of today's `persia-storage` container to `wasm32-unknown-unknown`.
That container uses file locks, `fdatasync`, and, when asked, `mmap`
([mapped reads, ADR-0017](../adr/0017-mapped-container-reads.md)). Those calls are the database. A WASM
port starts with a `Storage` implementation whose contract is honest about the host, then the engine on
top, which is already required to be storage-agnostic
([persia-engine lib.rs L1–L4](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/lib.rs#L1-L4)).
The engine stub does not make this cheaper: there is no engine to compile yet either.

Browser callers who need search before that exists run against the server (M11) through a small fetch
client. That client is not this package, and it is not designed here. It would be a third, thin, HTTP
wrapper over SPEC §12, closer to the M14 network clients than to an embedded binding.

## Packaging sketch

The meta package depends on the platform packages the way napi-rs projects do:

```json
{
  "name": "@persiadb/node",
  "optionalDependencies": {
    "@persiadb/node-linux-x64-gnu": "0.1.0-alpha.0",
    "@persiadb/node-linux-arm64-gnu": "0.1.0-alpha.0",
    "@persiadb/node-linux-x64-musl": "0.1.0-alpha.0",
    "@persiadb/node-darwin-x64": "0.1.0-alpha.0",
    "@persiadb/node-darwin-arm64": "0.1.0-alpha.0",
    "@persiadb/node-win32-x64-msvc": "0.1.0-alpha.0"
  }
}
```

npm installs the one package whose `os` and `cpu` match the machine. The others are optional, so a
platform that does not match does not fail the install. The JavaScript entry loads that `.node` file.
Users do not run `node-gyp` and do not install Rust.

The CI matrix, the release order, and the relationship to the Go static libraries are in
[distribution.md](./distribution.md).
