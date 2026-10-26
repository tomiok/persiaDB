# Embedded SDKs for Go and TypeScript

**Design only. Nothing here is implemented, and nothing here opens a database.**

Persia DB is pre-alpha. The public Rust API the bindings will wrap does not exist yet:
[`crates/persia/src/lib.rs`](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs#L1-L4)
is module docs and `#![forbid(unsafe_code)]`. There is no `Db`, no `Collection`, and no published crate
(`publish = false`). Live status is [`PROGRESS.md`](../../PROGRESS.md) (117 of 429 leaves on 2026-10-17).
When this directory and [`SPEC.md`](../../SPEC.md) disagree, the SPEC wins.

The decision this design follows is [ADR-0018](../adr/0018-embedded-sdk-bindings.md).

## Who this is for

Everyday Go and TypeScript developers who want the embedded database: open a file, index documents, get
ranked results. They are not expected to know Rust, the segment format, or cgo beyond what `go build` does
for them.

Two other audiences are explicitly secondary:

- Rust callers use the `persia` crate directly (ROADMAP M10). The bindings do not wrap that crate in a
  second Rust API.
- Operators of a cluster use the network clients (SPEC §13.2, ROADMAP M14): `sdk/rust` (`persia-client`)
  and `sdk/go` (`persia-go`). Those speak gRPC to `persia-server`. They are a different product, on
  different paths, and they are also not started.

## What exists today

| Piece | State | Where |
|---|---|---|
| Embedded facade `persia` | Stub. No types. | [lib.rs L1–L4](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs#L1-L4) |
| Engine | Stub. Docs say it will be synchronous and reach I/O only through `Storage`. | [persia-engine lib.rs L1–L4](https://github.com/tomiok/persiaDB/blob/main/crates/persia-engine/src/lib.rs#L1-L4) |
| Analysis, blobs, proto, CLI, server | Stubs. `persia-cli` and `persia-server` have an empty `main`. | [analysis](https://github.com/tomiok/persiaDB/blob/main/crates/persia-analysis/src/lib.rs#L1-L3), [blob](https://github.com/tomiok/persiaDB/blob/main/crates/persia-blob/src/lib.rs#L1-L3), [proto](https://github.com/tomiok/persiaDB/blob/main/crates/persia-proto/src/lib.rs#L1-L3), [cli](https://github.com/tomiok/persiaDB/blob/main/crates/persia-cli/src/main.rs#L1-L4), [server](https://github.com/tomiok/persiaDB/blob/main/crates/persia-server/src/main.rs#L1-L4) |
| On-disk codecs | Implemented in `persia-format`: header, frames, checksums, integer blocks, `BlobId` / `BlobRef`. They do not open a database. | [lib.rs L1–L40](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/lib.rs#L1-L40) |
| Storage | `Storage` trait, memory and local backends, the single-file container, opt-in mapped reads. No engine on top. | [storage lib.rs L1–L8](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/lib.rs#L1-L8), [trait L29](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/storage.rs#L29) |
| `sdk/` | Not created. | SPEC §13 |

Format version is 1.0 and unstable until the v1.0 release (SPEC §15).
[`FORMAT_MAJOR`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/header.rs#L15-L21)
is `1`, `FORMAT_MINOR` is `0`. There is no compatibility promise across releases until then.

## Goals

1. A Go program and a TypeScript program open the same `.persia` file the Rust API will open, in-process,
   with no server and no config file (SPEC §14).
2. The call surface covers the embedded operations SPEC §8.1 and §9.6 name: create and open a collection,
   put, update (upsert by id), delete, get, commit, search, blobs. Plus the schema (SPEC §6.2) and the
   relaxation report (SPEC §7.6).
3. Behavior matches the Rust core. Ranking, relaxation, and crash recovery are not reimplemented in Go or
   TypeScript (ADR-0001, ADR-0018).
4. Errors are ordinary values in Go (`error`, `errors.Is`) and exceptions with a stable `code` in
   TypeScript. Corruption is an error, never a crash of the binding itself (SPEC §17). A mapped-page fault
   is the exception the core already documents (ADR-0010): it kills the process.
5. The packages install the way those languages expect: a Go module, an npm package with prebuilt
   binaries. A Rust toolchain is for people building Persia, not for people using the SDK.
6. One conformance suite, shared with the Rust API, so a binding cannot quietly score a query differently.

## Non-goals

- Replacing the Rust crate, or hiding it. Rust remains the native API.
- The network clients, connection pools, retries, or shard routing. That is M14, and it waits on the server
  (M11) and on sharding (M13).
- A pure-Go engine, a TypeScript engine, or a WASM engine as the first release (ADR-0018).
- SQL, joins, multi-document transactions, or cross-collection queries (SPEC §1.2).
- Vector or semantic search (SPEC §1.2, §18.6).
- Text extraction. Callers pass text. Blob bytes stay opaque (SPEC §1.2, §9.2).
- Authentication. Embedded mode has none (SPEC §12.3). Cloud credentials, when a URL open exists, are the
  core's provider chain (SPEC §10.3), not a new SDK login API.
- A stable ABI or a stable file format before v1.0 (SPEC §15).
- Binding every `persia-format` codec. Callers who want those link the Rust crate. The SDK opens a database.

## Recommendation

| Language | First release | Later | Rejected as the primary path |
|---|---|---|---|
| Go | cgo, statically linked to `libpersia`, C ABI in `persia-ffi`. Module `sdk/persia`. | A no-cgo build only if a portable `Storage` exists and a host forbids cgo | Pure Go; wazero |
| TypeScript | N-API addon (`@persiadb/node`) for Node.js, Bun, and Deno, linking the same rlib | `@persiadb/wasm` for browser and edge, after a non-mmap storage backend | WASM as the only binding |

Both languages share `persia-ffi`: one safe Rust API, C exports for Go, N-API calling those Rust functions
for TypeScript. Documents cross as UTF-8 JSON. The reasons and the rejected options are in ADR-0018.

```
Go  --cgo-->  persia.h / libpersia.a --+
                                       +--> persia-ffi (safe Rust) --> persia --> engine
Node --N-API--> @persiadb/node (rlib) -+
```

`persia-ffi` is not in the workspace. When it is added it depends only on `persia`, and that edge is added
to [`ALLOWED`](https://github.com/tomiok/persiaDB/blob/main/xtask/src/check_deps.rs#L23-L40). The Node
package stays outside the Cargo workspace so its build script does not fight ADR-0003.

## Documents

| Doc | What it decides |
|---|---|
| [ADR-0018](../adr/0018-embedded-sdk-bindings.md) | The binding strategy |
| [C ABI](./c-abi.md) | Handles, ownership, errors, threads, panics, the function list |
| [Go](./go.md) | The Go API, `context.Context`, errors, cgo packaging |
| [TypeScript](./typescript.md) | The TypeScript API, Promises, workers, N-API and WASM |
| [Distribution and testing](./distribution.md) | Versions, the file format, npm and Go modules, CI, conformance |

The website summary is the guide page "Embedded SDKs", which links here.

## Phased roadmap

The phases below are the design. The checkboxes that implement them are ROADMAP M16 (target v1.1): the C ABI,
the Go module, and `@persiadb/node`. Phase 7, the WASM package, is M28 (target v2.3), which starts with the
portable storage backend this table asks for. M14 stays the network clients. A binding still cannot be tested
before `Db` exists (M10). M16 is blocked on v1.0 (M15) so the shipped ABI sits on the frozen format.

| Phase | When it can start | What the bindings gain | Blocked on |
|---|---|---|---|
| 0. This design | Now | Docs and ADR-0018 only | — |
| 1. ABI skeleton | After ROADMAP 10.1.1, 10.1.2, 10.1.5 | `open` / `close`, create and open a collection, error codes, poison-on-panic. Still no search. | `Db` and a stable Rust error type. Schema creation also needs M3 (collections), which M10 already depends on. |
| 2. Documents and search | After 10.1.3, 10.1.4, and M7–M8 | `put`, upsert, `delete`, `get`, `put_batch`, `commit`, `search` with the relaxation object (SPEC §7.6). M10 depends on M8, so the first core release already relaxes queries. The SDK reports that from its first search release. | The engine. Analyzers are M4. |
| 3. Blobs | After 10.1.3's blob methods and M9 | `put_blob`, ranged reads, the pull reader in the [C ABI](./c-abi.md). M9 is already a dependency of M10, so this can ship in the same binding release as phase 2 or as the release after it. | Blob store, including the upload-staging question (SPEC §18.8). |
| 4. Options and shutdown | After 10.2.3 and SPEC §14 knobs existing on `OpenOptions` | Memtable size, fsync policy, thread counts, `relax.*`, clean shutdown of flush and merge threads. Until a knob exists in Rust, the SDK does not invent one. | The Rust builders. |
| 5. URL open | After 10.1.1 accepts a URL and M12 has a backend | Pass `s3://`, `gs://`, `az://` through. Credentials stay the core's chain (SPEC §10.3). | Cloud backends. |
| 6. Publish | After M16.1–M16.3, and only once the v1.0 tag exists (M15). M16.4 is this phase: publishing, the compatibility table, and the package builds | Modules and npm packages, prebuilt binaries, the compatibility table. File format, C ABI, and package versions stay distinct (ADR-0018). | The v1.0 format freeze, and sections 16.1–16.3. |
| 7. WASM package | ROADMAP M28 (target v2.3), after 28.1 | `@persiadb/wasm` for browser and edge. | A `Storage` that does not need mmap or POSIX fsync (M28.1). Do not start it by compiling `persia-ffi` to `wasm32` and hoping the container works. |
| — | M11–M14 | Not this SDK. Network clients, retries, shard-aware writes. | Server and distribution. |

Phase 1 can be a crate and a header with tests that open nothing useful. It is still worth landing only
once `persia` has a type to wrap; an FFI crate over a stub facade tests the panic boundary and little else.

## Open questions

Collected here. The language docs repeat one only when the answer changes that language's API.

1. **npm scope.** `@persiadb/node` assumes the scope is available. Confirm before the first publish. The
   directory `sdk/node` does not depend on the answer.
2. **Where `commit` sits.** The README sketch and ROADMAP 10.1.3 put `commit` on the collection. The
   manifest is a database snapshot (SPEC §2, §8.2). This design follows the sketch: `Collection.Commit`
   publishes the database manifest. If the Rust API puts `commit` on `Db`, the bindings follow Rust and
   this paragraph is updated in that PR.
3. **Is `put_batch` atomic?** SPEC §8.1 names it and does not say. The SDK method will match whatever
   10.1.3 specifies. Until then the design does not promise all-or-nothing.
4. **JSON spelling of `datetime`.** SPEC §6.1 stores it as i64 microseconds UTC and does not say how JSON
   spells it. Proposal, to be confirmed when M3 defines the document JSON: an RFC 3339 string in UTC, which
   the core converts. The SDK will not keep a private spelling if M3 chooses another.
5. **Missing `get`.** Proposal: Go returns a sentinel `ErrNotFound`; TypeScript `get` returns `null`; a
   corrupt file is an error in both. The Rust method will be `Result`, and 10.1.5 assigns the code. If Rust
   uses a different split, the bindings follow it.
6. **Empty document id.** SPEC §6.1 says `id` is a string of at most 512 bytes. It does not forbid empty.
   The SDK passes the id through and surfaces the core's error.
7. **Cursor across a merge.** SPEC §7.4's `search_after` cursor is `(score, segment_gen, docid)`. A merge
   rewrites segments. What a stale cursor returns is a core question. The SDK returns that error as-is.
8. **Cancellation.** The engine is synchronous (SPEC §3 principle 3, [storage.rs L12–L14](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/storage.rs#L12-L14)).
   `context.Context` and `AbortSignal` are checked before the native call. A cancel that arrives during
   the call does not stop it, until the Rust API grows a cancel flag. See the Go and TypeScript docs.
9. **Mapped reads in the host.** Default remains ordinary reads (ADR-0017). Whether `OpenOptions` exposes
   the opt-in is decided with 10.1.1. The bindings pass the flag through; they do not default it on.
10. **In-process sharing.** Several readers, one writer (SPEC §8.4). Two runtimes in one process (a Go
    program that also embeds Node, or two Node worker threads) each opening the path read-write will trip
    the writer lock. The SDK documents that. It does not add a process-wide singleton.
11. **Conformance file format.** ROADMAP 14.1.1 will pick a YAML shape for the network clients. The
    embedded suite should be that shape plus a `transport` field, so scenarios are written once. Settle it
    when 14.1.1 is written, not before.
12. **Windows cgo CRT and Linux musl** for the Go static library. The first Go matrix is gnu Linux, macOS,
    and mingw Windows. musl is a fast follow. Details in [distribution](./distribution.md).
