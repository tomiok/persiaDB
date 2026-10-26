# Go SDK (embedded)

**Design only.** No Go module, no cgo wrapper, and no function you can call. Import paths below 404 until
the module exists. The network client planned at `sdk/go` (module `persia-go`, ROADMAP 14.3) is a
different library and is not this API.

The binding strategy is [ADR-0018](../adr/0018-embedded-sdk-bindings.md). The C functions this package
wraps are specified in [c-abi.md](./c-abi.md). Packaging, versions, and CI are in
[distribution.md](./distribution.md).

## Module

| | |
|---|---|
| Path | `sdk/persia` |
| Module | `github.com/tomiok/persiaDB/sdk/persia` |
| Package | `persia` |
| First import | `persia.Open` |

Application code never imports `C` and never calls `persia_bytes_free`. The package boundary is the
place cgo is allowed.

A program that also talks to a remote cluster imports this package as `persia` and the future network
module under another name. ROADMAP 14.3 has not picked that package name. This design asks it to be
`client` (or any name other than `persia`) so the embedded call reads as `persia.Open`.

## What a caller writes

Illustrative. These methods are not in a repository, and the Rust methods they will wrap are not either
([`persia` lib.rs](https://github.com/tomiok/persiaDB/blob/main/crates/persia/src/lib.rs#L1-L4)).
They follow SPEC §6.2, §7.1, §7.6, §8.1, and §9.6, and ROADMAP 10.1.

```go
package main

import (
    "context"
    "fmt"
    "log"

    "github.com/tomiok/persiaDB/sdk/persia"
)

func main() {
    ctx := context.Background()

    db, err := persia.Open(ctx, "app.persia", persia.WithCreate())
    if err != nil {
        log.Fatal(err)
    }
    defer db.Close()

    products, err := db.CreateCollection(ctx, persia.Schema{
        Name: "products",
        Fields: map[string]persia.Field{
            "title": {Type: persia.Text, Analyzer: "en", Boost: 3},
            "body":  {Type: persia.Text, Analyzer: "en"},
            "brand": {Type: persia.Keyword, Facet: true},
            "price": {Type: persia.F64},
            "sales": {Type: persia.I64},
        },
        Relaxation: persia.RelaxationPolicy{Enabled: true, MinResults: 1},
    })
    if err != nil {
        log.Fatal(err)
    }
    defer products.Close()

    // Upsert by id (SPEC §8.3). A second Put with the same id is the update.
    err = products.Put(ctx, Product{
        ID: "sku-1", Title: "Running shoes", Brand: "nike", Price: 80, Sales: 12,
    })
    if err != nil {
        log.Fatal(err)
    }
    if err := products.Commit(ctx); err != nil {
        log.Fatal(err)
    }

    // The typo is the query. Relaxation is on by default (SPEC §7.6).
    results, err := products.Search(ctx, persia.Text("runing"))
    if err != nil {
        log.Fatal(err)
    }
    for _, hit := range results.Hits {
        fmt.Println(hit.ID, hit.Score)
    }
    if results.Relaxation != nil {
        fmt.Println(results.Relaxation.Level, results.Relaxation.Corrected)
    }
}
```

`Product` is the caller's struct. The package encodes it as the document JSON from the
[C ABI](./c-abi.md). The `ID` field is the document id; `json:"id"` works the same for a struct the
caller already has. There is no generated schema type and no `go generate` step in the first release.

## API

`context.Context` is the first argument after the receiver, on every call that crosses into C. `Close`
takes no context: it must run on the way out, including when the caller is already cancelled.

```go
func Open(ctx context.Context, path string, opts ...Option) (*DB, error)
func (db *DB) Close() error
func (db *DB) CreateCollection(ctx context.Context, schema Schema) (*Collection, error)
func (db *DB) Collection(ctx context.Context, name string) (*Collection, error)
func (db *DB) DropCollection(ctx context.Context, name string) error

func (c *Collection) Put(ctx context.Context, doc any) error
func (c *Collection) PutBatch(ctx context.Context, docs []any) error
func (c *Collection) Delete(ctx context.Context, id string) error
func (c *Collection) Get(ctx context.Context, id string, dest any) error
func (c *Collection) Commit(ctx context.Context) error
func (c *Collection) Search(ctx context.Context, q Query) (*Results, error)
func (c *Collection) Schema(ctx context.Context) (Schema, error)
func (c *Collection) Close() error
```

`Option` values cover `WithCreate`, `WithReadOnly`, and, once Rust `OpenOptions` has them (ROADMAP
10.1.1, SPEC §14), the knobs the core actually implements. A knob the core does not have is not added
here first.

### Documents

`Put` and `Get` accept `any` that `encoding/json` can handle.

- A struct with `ID string` or `json:"id"`.
- `map[string]any`.
- `json.RawMessage`, passed through after a parse that checks it is an object and has a string `id`.

`Get` writes into `dest` the way `json.Unmarshal` does. A missing document returns `ErrNotFound` and
leaves `dest` untouched. A corrupt database returns `ErrCorrupt`, not a zero struct.

Field types follow SPEC §6.1. The schema is the authority: a Go `int` encoded as JSON for an `i64`
field is fine; a `float64` with a fraction for an `i64` field is `ErrInvalid`. `datetime` waits on the
JSON spelling M3 chooses (see the [index](./README.md)).

`Put` of an existing id is the update. There is no separate `Update`, because the core's operation is
an upsert (SPEC §8.3) and a second method would imply a difference that does not exist.

`PutBatch` encodes one JSON array and one C call. Whether the batch is atomic is unspecified (SPEC
§8.1); the method will match ROADMAP 10.1.3 when that leaf defines it. Callers who need a durable point
call `Commit` after the batch.

### Search

```go
type Query struct {
    Q            string
    Fields       []string          // "title^3"; empty means the schema boosts (SPEC §7.1)
    Filter       Filter            // omitted means no filter
    Sort         Sort              // default score
    Limit        int               // default 20
    Offset       int
    SearchAfter  *Cursor
    Facets       []string
    Relax        *Relax            // nil means the collection default; RelaxOff() disables
    Explain      bool
}

func Text(q string) Query

type Results struct {
    Hits         []Hit
    Facets       map[string][]FacetBucket
    Relaxation   *Relaxation // nil when the cascade did not run
    ManifestSeq  uint64
}

type Hit struct {
    ID     string
    Score  float64
    Fields map[string]any
    Blob   *BlobRef
    Cursor *Cursor // search_after value for the next page (SPEC §7.4)
}

type Relaxation struct {
    Level     string            // "exact" | "synonyms" | "drop_terms" | "prefix" | "fuzzy" | "fallback"
    Applied   []string
    Original  string
    Effective string
    Corrected map[string]string
    Truncated bool
}
```

`Text("runing")` is the common case. The package sends `{"q":"runing"}`. The cascade (SPEC §7.6) is
the core's job: exact, then synonyms, then dropping terms, then prefix, then edit distance, then the
collection fallback. The `Relaxation` value is how the caller sees what changed. Stricter levels rank
above looser ones. Filters are not relaxed unless `Relax.Filters` is `"last_resort"`.

`Explain: true` is accepted in the struct and returned as an error (`ErrUnsupported`) until SPEC §7.7
exists. The field is on the struct so a later minor does not rename the call.

An empty `Hits` with a nil error is a real answer. It happens when relaxation is off, the fallback is
`none`, or the budget stops the cascade (`Truncated: true`).

### Pagination

There is no iterator that holds a snapshot open across the C boundary
([c-abi.md](./c-abi.md)). `Results` is one page.

```go
q := persia.Text("shoes")
q.Limit = 20
for {
    page, err := products.Search(ctx, q)
    if err != nil {
        return err
    }
    if len(page.Hits) == 0 {
        return nil
    }
    // ... use page.Hits ...
    last := page.Hits[len(page.Hits)-1]
    q.SearchAfter = last.Cursor
}
```

`Hit.Cursor` is the `search_after` value from SPEC §7.4 (`score`, `segment_gen`, `docid`). The loop
above reissues the query. A commit that lands between pages can change the next page. A cursor a merge
has invalidated returns the core's error. `Offset` exists for shallow pages and is the wrong tool for
a deep walk, which is why the SPEC prefers the cursor.

`Search` does not return a total hit count. The engine collects top-k (SPEC §7.3). A count API waits
until the Rust API has one.

### Blobs

Blocked on M9. The shape, once SPEC §9.6 and §18.8 are implemented:

```go
ref, err := products.PutBlob(ctx, bytes.NewReader(file), persia.BlobMeta{MIME: "application/pdf"})
doc.Blob = ref
err = products.Put(ctx, doc)

r, err := products.Blob(ctx, ref.ID, persia.Range{Offset: 0, Length: 1 << 20})
defer r.Close()
_, err = io.Copy(dst, r) // r is an io.ReadCloser; Read fills a caller buffer
```

Until the staging ADR exists, `PutBlob` reads the `io.Reader` to completion on the Go side and makes
one C call with that buffer. The signature stays `io.Reader` so a later implementation can stream
chunks without an API change, once SPEC §18.8 says how an id that is known only after the last byte
is stored. Callers with multi-gigabyte files should wait for that decision. The download side is a
pull reader from the first blob release: `Read` fills a caller buffer (SPEC §9.2 range reads).

The package helper `PutDocumentWithBlob` performs the two steps in the SPEC §9.5 order: blob durable,
then the document. A crash between them leaves an orphan blob, which the core treats as harmless.

`BlobRef.ID` is 64 lowercase hex characters, the text form of SHA-256
([`BlobId`](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/blob.rs#L19-L43)).
The caller does not compute it. If the caller passes an expected id and the bytes disagree, the core
returns an error (SPEC §9.1).

### Transactions and commits

There is no `Begin` and no `Rollback`. Multi-document transactions are out of scope (SPEC §1.2).

`Commit` makes the write durable and publishes a manifest (SPEC §8.2, local protocol SPEC §4.5). A
`Put` that has returned is already visible to `Search` on that database before `Commit`
(read-your-writes, SPEC §8.2–§8.4). Losing the process before `Commit` loses the uncommitted WAL tail
the core has not synced. The default durability of `Commit` is `Fsync` (SPEC §8.2). The SDK does not
weaken that default.

`commit_interval` and the other SPEC §14 knobs are passed through when `OpenOptions` exists. They are
not hardcoded in Go.

`Close` on the database asks the core to stop background threads (ROADMAP 10.2.3) and drops the writer
lock. `Close` on a database that still has open collections returns `ErrBusy`. `defer` in the reverse
order of acquisition: collections, then the database.

## Errors

```go
var (
    ErrNotFound       = errors.New("persia: not found")
    ErrAlreadyExists  = errors.New("persia: already exists")
    ErrConflict       = errors.New("persia: writer conflict")
    ErrInvalid        = errors.New("persia: invalid argument")
    ErrCorrupt        = errors.New("persia: corrupt")
    ErrUnsupported    = errors.New("persia: unsupported format")
    ErrIO             = errors.New("persia: io")
    ErrTransient      = errors.New("persia: transient")
    ErrBusy           = errors.New("persia: busy")
    ErrPanic          = errors.New("persia: panic")
    ErrClosed         = errors.New("persia: closed")
    ErrLimit          = errors.New("persia: limit")
)

type Error struct {
    Code Code   // the persia_status value
    Msg  string // not stable
    Err  error  // the sentinel above
}

func (e *Error) Error() string
func (e *Error) Unwrap() error
```

Callers use `errors.Is(err, persia.ErrNotFound)`. The message can change. The sentinel cannot, within
an ABI major. Codes match the table in [c-abi.md](./c-abi.md).

`ErrPanic` means the Rust stack unwound and the handle is poisoned. Further calls fail the same way
until `Close`. `Close` still releases the lock. This is a bug in the core or the binding, not a corrupt
file. Corrupt files are `ErrCorrupt`.

`ErrConflict` is the second writer (SPEC §8.4). The call fails immediately; it does not wait for the
other process.

The package does not retry `ErrTransient`. Search is safe to retry. Put is not, unless the caller
knows the id's new contents are the same as the attempt (upserts of the same bytes are the closest
thing to idempotent, and even that is the core's upsert, not a retry loop in Go).

## context.Context

The engine is synchronous. The storage trait's own comment says asynchronous backends block inside the
call
([storage.rs L12–L14](https://github.com/tomiok/persiaDB/blob/main/crates/persia-storage/src/storage.rs#L12-L14)).
The optional Rust `tokio` wrapper is `spawn_blocking` (ROADMAP 10.1.6), which also does not cancel the
work.

So:

- If `ctx.Err()` is already non-nil, the package returns it and does not cross into C.
- Once the C call has started, cancellation does not stop it. The call runs to completion, and the
  package returns the operation's error if it failed, or `ctx.Err()` if the context ended and the
  operation succeeded. The operation has still happened. A cancelled `Put` may have been applied.
  Callers who need to know call `Get`.
- `context.WithTimeout` is therefore a deadline on *starting* and on *observing* the result, not a
  preemption of BM25. Query-time budget is `relax.budget_ms` (SPEC §7.6, default 50 ms), set on the
  query, and it is the core that stops the cascade and sets `Truncated`.
- The package does not start the C call on another goroutine in order to select on `ctx.Done`. That
  would leak a goroutine blocked in cgo for the rest of the call and invite a use of the handle after
  the caller gave up.

When the Rust API grows a real cancel flag, `Open` and `Search` will pass it through and this section
changes. Until then the limitation is part of the godoc on `Open`, not a footnote.

Deadlines and retries for a **remote server** belong to the network client (ROADMAP 14.3.5), which has
an RPC to cancel. Importing this package does not bring them.

## cgo, threads, and the process

The package is safe for concurrent use from many goroutines. The ABI serializes writers and allows
concurrent readers ([c-abi.md](./c-abi.md)). A cgo call occupies an OS thread for the duration; a long
search holds one thread. That is acceptable because the call is coarse (a query, not a posting).

`GOMAXPROCS` goroutines blocked in cgo can stall the Go scheduler if every thread is inside a search.
The core already has its own search thread pool (SPEC §7.3, `search_threads` in SPEC §14). The package
documents that callers should not set that pool larger than the machine and then also fan out unbounded
goroutines into `Search`.

The writer lock is per database path, across processes. Two `Open` calls for write on the same path
fail the second with `ErrConflict`, whether they are two processes or two `DB` values.

### What can kill the process

A panic in Rust becomes `ErrPanic`. A fault on a mapped page does not: it raises `SIGBUS` (Unix) or
`EXCEPTION_IN_PAGE_ERROR` (Windows) and the process ends
([mmap.rs L1–L8](https://github.com/tomiok/persiaDB/blob/main/crates/persia-format/src/mmap.rs#L1-L8),
ADR-0010). Mapped reads are off unless the caller opts in, and the default in the core today is
ordinary reads (ADR-0017). The Go package will not opt in by default.

Go and Rust in one process both have opinions about `SIGSEGV` / `SIGBUS`. Before `WithMappedReads` is
documented as supported, the binding is tested on linux, darwin, and windows with the option on, under
`go test` and under a fault injected in a child process. If Go's signal handler swallows the fault or
deadlocks, the option stays unavailable in the Go SDK and the godoc says so. The database still works
through `pread`.

## Build

Everyday install, once a release exists. The module tag follows the core `v1.0.0` tag (M15, then M16.4),
not `v0.1.0-alpha` (ROADMAP 10.5.2, the core's notice that the format is still unstable):

```bash
go get github.com/tomiok/persiaDB/sdk/persia@v1.0.0
```

`go build` needs cgo (`CGO_ENABLED=1`) and a C linker. It does not need Rust, because the module
downloads a checksummed `libpersia.a` for the current `GOOS`/`GOARCH` into the build cache. The first
matrix is linux amd64, linux arm64, darwin amd64, darwin arm64, and windows amd64 (mingw, which is the
toolchain Go's cgo uses on Windows). musl is not in that first matrix. Details, the source-build tag,
and `PERSIA_LIB_DIR` are in [distribution.md](./distribution.md).

`go test -race` is part of CI for this module. The race detector does not see inside the Rust static
library. Data races on the Go side of the wrapper are what it catches; races inside the engine stay the
Rust tests' job (ROADMAP 10.2).

## Small example: delete, reopen, read-your-writes

```go
if err := products.Delete(ctx, "sku-1"); err != nil {
    return err
}
// Visible to Search now, before Commit (SPEC §8.2).
// Not durable across a crash until Commit returns.
if err := products.Commit(ctx); err != nil {
    return err
}

var got Product
err := products.Get(ctx, "sku-1", &got)
if errors.Is(err, persia.ErrNotFound) {
    // deleted
}
```

Reopening the path in another process sees the committed manifest and does not see an uncommitted put.
That is the core's recovery (SPEC §8.6), tested in Rust. The Go conformance adapter checks it by
committing, closing, reopening, and searching. It does not reimplement crash injection; the core's
`FaultyStorage` and container tests already do that.
