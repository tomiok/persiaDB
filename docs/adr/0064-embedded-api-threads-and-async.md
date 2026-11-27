# ADR-0064: Threads of the embedded API: one maintenance thread per read-write `Db`, a bounded `Drop`, and an async API we own

- Status: proposed
- Date: 2026-11-23
- Related: SPEC §8.1, §8.2, §8.4, §8.5, §14; ROADMAP 10.1.6, 10.1.11, 10.2.1–10.2.3, 10.2.6; ADR-0029, ADR-0032,
  ADR-0063; `docs/sdk/c-abi.md`

## Context

The engine commits and merges only when it is driven: `Database::tick` commits what the commit policy finds due,
`Database::merge_step` commits finished merges and starts new ones, and both take `&mut self` (ADR-0029, ADR-0032).
An application that embeds Persia must not have to call either, and its `put` must not be the call that happens to
pay for a merge commit. The public `Db` is `Send + Sync` with every method on `&self` (ADR-0063), so it needs a
writer mutex, and something must decide what happens to background work when a `Db` is closed, dropped, or dropped
while a thread is still inside a call. Separately, ROADMAP 10.1.6 asked for an async wrapper as a `tokio` feature,
and no crate of the embedded stack may depend on an async runtime. The owner decided these on 2026-11-23 (wave 7,
decisions d, e and o).

## Decision

### Threads (implemented in wave 7)

1. **A writer mutex.** A read-write `Db` holds its engine database behind one mutex. `put`, `put_batch`, `delete`,
   `commit`, and the collection changes each take it for one call: writes are ordered, not transactional. Reads
   never take it: `get` loads the published snapshot, which the engine swaps atomically (SPEC §8.4), and blob reads
   go through a shared handle on the pack. A blob's bytes are read and stored before the mutex is taken.
2. **One maintenance thread per read-write `Db`**, started by the open and named `persia-maintenance`. None per
   collection. None on a read-only `Db`, whose `refresh()` is explicit. Merge workers stay the engine's pool
   (`merge_threads`); the maintenance thread is what drives them.
3. **Rounds.** A round is one `tick` and one `merge_step`, under the writer mutex. What is due in a round is the
   engine's `CommitPolicy` on the database's clock: a memtable at its size, idle for 10 s, or 60 s after its first
   unflushed write (SPEC §14 defaults). A commit and a merge commit therefore run off the application's threads.
4. **Parking.** While anything is unflushed or a merge is in flight, the thread runs a round every second. After a
   round that leaves neither, it parks with no timer until the next write. An open starts it active, because the
   open may have replayed writes. A write that arrives while a round runs is counted, so the round does not park
   over it.
5. **`close(self)`** sets the closed flag (every later call on any handle fails with `Closed`), tells the thread to
   stop, and waits for it without limit. The thread makes the final commit, cancels the merges in flight, drops the
   database (the pack before the container, ADR-0053), which releases the writer lock, and sends the first error:
   the first failure of a background round since the open, else the final commit's. `close` returns it.
6. **`Drop`** makes the same request and waits up to 5 s for the result (a channel and `recv_timeout`: a
   `JoinHandle` has no timed join). On time, an error is logged with `tracing::error!`. Past the bound it detaches
   the thread and logs: the thread still makes the final commit and releases the lock when the call that holds the
   writer mutex returns. **A detached thread keeps the writer lock until it ends**, so a reopen right after such a
   drop can fail with `Conflict`: callers should prefer `close()`. `Drop` never panics.
7. **Errors in the background.** A round that fails records the first error for `close`. If the failure poisoned
   the engine, every later write fails with `Poisoned` and the thread parks. A panic on the maintenance thread, or
   on any thread while it holds the writer mutex, poisons the mutex: later writes fail with `Poisoned`, reads keep
   working on the last published snapshot, and `close` or `Drop` then drops the database with no final commit (what
   the memtables hold after a panic is unknown) and reports `Poisoned`. A panic never propagates out of `close` or
   `Drop`.
8. **Handles are `'static` and `Closed`-aware.** `Collection` and `BlobReader` own an `Arc` of the database and no
   lock, so they can move to other threads and outlive the `Db`; after a close they fail with `Closed`, and the file
   can be reopened while they exist.
9. **Tests are deterministic.** The clock is injected (`ManualClock`, a hidden `OpenOptions::clock`), a hidden
   `Db::maintain_now()` asks the thread for one round and waits for it, a hidden `OpenOptions::hold_maintenance`
   lets the thread run only those requested rounds (no timed round can interleave), a hidden
   `Collection::hold_writer_while` holds the writer mutex as a long write would, and the `Drop` bound
   (`OpenOptions::drop_wait`) and the place a detached thread is put (`OpenOptions::detached_into`, so a test can
   join it) are hidden options. `Db::committed_seq` shows the latest commit. No test sleeps.

### Async (decided, implemented in wave 8: ROADMAP 10.1.11)

10. **No async runtime in the embedded stack.** `persia`, `persia-engine`, `persia-blob`, `persia-storage`,
    `persia-format`, `persia-analysis` and `persia-cli` have no `tokio`, `async-std` or `smol` in their resolved
    dependency graph, dev-dependencies included; `cargo xtask check-deps` enforces it (ROADMAP 0.6.11).
    `persia-server` may use tokio, axum and tonic, and uses the embedded crates only through their synchronous API.
11. **A runtime-agnostic async API that we own** replaces ROADMAP 10.1.6's `spawn_blocking` wrapper: `AsyncDb` and
    `AsyncCollection` behind the cargo feature `async`, which adds no dependency. They offer `open`, `close`,
    `put`, `put_batch`, `delete`, `get`, `commit` and `search`.
12. **Its own pool.** Each `Db` gets a pool of plain threads on first async use, `min(4, cores)` by default, joined
    on `close`, and not shared with the maintenance thread. A future is a handle on a job queued to that pool.
13. **Cancellation.** A future dropped before its job starts skips the job. A write that has started runs to its
    end and its result is discarded. A search or a get that has started has its `CancelToken` set. Jobs still
    queued at `close` complete with `Closed`.
14. **Tested without a runtime:** with a `block_on` written in the test. No runtime is a dev-dependency.
15. ROADMAP 10.1.6 is ticked "Done as: 10.1.11" only when 10.1.11 lands. Wave 7 only keeps the handles `'static`
    and `Closed`-aware (item 8), which is what lets a pool move them.
16. **As implemented (wave 8, 2026-11-25).** Items 11 to 14 hold as written; what they left open was settled so:
    - Every call returns `JobFuture<T>`, one named, `Unpin` future type (not `impl Future`), so callers can store
      it. Its job is queued when the method is called, not at the first poll: what "before its job starts" means
      in item 13 does not depend on when the caller first polls. Polled again after `Ready`, it returns
      `InvalidArgument`.
    - The pool belongs to the `AsyncDb` (one per `Db`) and its `AsyncCollection`s. `open` and `close` do not run on
      it: no pool exists before the open, and the close joins it. Each runs on a thread of its own, joined when its
      future takes the result; a dropped `open` future lets the open finish and closes what it opened, and a dropped
      `close` future lets the close finish.
    - Besides the eight calls, `AsyncDb` has `open_with(OpenOptions, location)` (an open needs its options) and a
      synchronous `collection(name)` (a lookup in the published state, which takes no lock). Collection management,
      blobs, `refresh` and the JSON and typed variants are not async yet (ROADMAP 10.1.26).
    - A get that has started checks its token before it reads: a get is one lookup, with no point to stop at inside.
    - Dropping an `AsyncDb` without `close` closes the pool (later jobs `Closed`, queued ones completed with
      `Closed`) but does not join its threads, which end after the job each runs: a destructor does not wait behind
      a long write. The `Db` inside is then dropped as item 6 says.
    - A job that panics resolves to `ErrorCode::Io` (an internal invariant that did not hold): `Panic` is the C
      ABI's alone. A panic inside a write still poisons the writer (item 7).
    - The pool size is a hidden test option (`OpenOptions::async_threads`); a public one is ROADMAP 10.1.27.

## Consequences

- A program that only opens, puts and closes names neither `tick` nor `merge_step`, and its data is committed by
  the idle flush, the interval, or the close.
- A `put` never pays for a merge commit. It can still pay for a flush commit: a memtable at its size threshold is
  flushed by the write that finds it full (SPEC §8.2 step 3), as in the engine.
- A long write (a large `put_batch`) delays the round behind it, and delays `close` and `Drop`. A blob upload delays
  no write and no round, since it runs before the mutex is taken, but it does delay `close` and `Drop`: the upload
  holds the read side's share of the pack, which the close drops before the container (ADR-0053 item 2), so the pack
  never outlives the writer lock its proof stood for.
- One idle thread per open read-write database. A process with thousands of open databases has thousands of
  parked threads; a shared pool would be a later change behind the same API.
- After a bounded `Drop` gave up, nothing reports the final commit's failure but the log.
- The 1 s period is not configurable. It bounds how late an idle flush or an interval commit can be.
- **The parents `Db::open` creates** (ROADMAP 10.1.9) are made durable by the engine's `create_parent_dirs`, which
  syncs each new directory's parent (SPEC §4.5.2), so a power loss right after the first commit cannot lose the file's
  directory entry. (`std::fs::create_dir_all`, which syncs nothing, was used until that helper reached `main`.)
- **Cloud backends and 0.6.11.** ROADMAP 12.1.1 plans `object_store` in `persia-storage`, and `object_store` needs
  tokio: under the no-runtime rule (item 10) it cannot be a dependency of an embedded crate as it is. Where the cloud
  backends' async I/O lives (a separate crate beside `persia-server`, a runtime-free client, or a change of the rule)
  is decided in M12's step 0 (owner decision o); 12.1.1 is unchanged until then.

## Alternatives considered

- **Commit and merge on the caller's thread** (the engine's model, with `put` calling `tick`). No thread, but an
  idle database never flushes, and one unlucky `put` pays for a merge commit.
- **A thread per collection, or a process-wide pool.** The writer mutex is per database, so per-collection threads
  would only queue on it; a process-wide pool couples the shutdown of unrelated databases.
- **`Drop` that joins without limit.** A destructor that can block forever behind another thread's call.
  **`Drop` that never waits.** Then a `drop(db)` followed by an open of the same path would usually fail on the
  writer lock.
- **An optional `tokio` feature** (ROADMAP 10.1.6 as written). It would put a runtime in the embedded crate's
  dependency graph and bind the API to one runtime.

## Amendment (2026-11-27): the async pool's size is a public option (ROADMAP 10.1.27)

Item 12 fixed the pool at `min(4, cores)` threads, and wave 8 had its size only as a hidden test hook. The owner
decided:

1. **`OpenOptions::async_threads(threads: usize)`**, public and documented, behind the feature `async` (no such
   setter without it). It takes a plain `usize`, so that 0 can be written: an open (`OpenOptions::open`, and so
   `AsyncDb::open_with`) with 0 fails with `ErrorCode::InvalidArgument` before it creates or reads anything, the way
   `memtable_bytes(0)` does, never a panic. The default stays `min(4, cores)` (`available_parallelism`, 1 when the OS
   does not say). The option is per open, like every other: nothing is stored in the database.
2. **No upper bound.** A count the OS cannot start is not refused at open; it fails the first job instead (item 3).
   No SPEC rule or measurement gives a bound to enforce, and a bound would refuse a machine that can run more. The
   pool does not preallocate for the count, so a huge one (`usize::MAX`) fails as the OS refuses a thread, never as an
   allocation. A refused start is not remembered: each later job tries the whole start again, under the pool's lock,
   so a `close` or a `Drop` that needs that lock waits for one such attempt, and a count the OS can never start costs
   every job a failed start. Accepted: retrying lets a pool start once the OS has threads again.
3. **All or nothing, never a panic.** The first job starts every thread of the pool. If the OS refuses one
   (`std::thread::Builder::spawn` returns an `io::Error`), that job resolves to `ErrorCode::Io` with the `io::Error`
   as its `source()`, and did not run. The threads that this start had already spawned end, and are joined before the
   call returns: each sees, under the pool's lock and before it takes a job, that the start's generation moved on.
   So a failed start leaves no thread behind. The next job tries the whole start again. (Wave 8 kept the threads
   that started and failed only a pool of none; a pool of fewer threads than set is no longer possible.) A pool given
   0 threads in code that bypasses the open's check refuses its jobs with `InvalidArgument` instead of queueing them
   for ever.
4. **Tested through hidden test hooks** (feature `test-hooks`, not in the public-API snapshot): `AsyncDb::pool_threads`
   (the threads the pool has installed: 0 until a start succeeds, the size set after it; a refused start installs
   none, by construction) and `OpenOptions::async_refuse_thread(n)` (the OS taken to refuse thread `n` at the pool's
   first start). `crates/persia/tests/async_api.rs`, and `async_lifecycle.rs`, whose census of the process's threads
   on Linux is what shows that a refused start's threads ended.
5. **No Go, TypeScript or C form yet.** The embedded bindings of ADR-0018 do not expose the async API (their calls
   are synchronous, and each language has its own way to run them off its event loop), so the option has no form
   there; it gets one with them if they ever expose async.

## Amendment (2026-11-27): the rest of the embedded API as async (ROADMAP 10.1.26)

Item 16 left collection management, blobs, `refresh`, `backup` and the JSON and typed variants without an async form,
so an async program created its collections through a `Db` first. The owner decided which get one. Every new call
runs on the same pool as item 11's (item 12), returns a `JobFuture<T>`, is queued when it is called, and follows item
13: a future dropped before its job starts skips it and leaves nothing; a job still queued at `close` completes with
`Closed`. What dropping the future of a job that has started does is per call, below. Arguments are owned, since the
job runs on another thread.

| Call | Form | Dropping the future once the job has started |
|---|---|---|
| `AsyncDb::create_collection(CollectionSchema)` | async, resolves to an `AsyncCollection` | runs to its end (created and committed) |
| `AsyncDb::create_collection_json(impl Into<String>)` | async; the JSON is read on the pool | runs to its end |
| `AsyncDb::collection_names()` | synchronous on `AsyncDb`: a lookup in the published state, which does no I/O of its own, as `AsyncDb::collection` (item 14) | (no job) |
| `AsyncDb::drop_collection(impl Into<String>)` | async | runs to its end (dropped and committed) |
| `AsyncDb::put_blob<R: Read + Send + 'static>(R, PutBlobOptions)` | async, owned reader | runs to its end (the blob is stored whole) |
| `Db::get_blob` | not offered | |
| `AsyncDb::get_blob_range(BlobRef, Range<u64>)` | async, a read | token set; checked before the read |
| `AsyncCollection::put_with_blob<R: Read + Send + 'static>(Document, R, PutBlobOptions)` | async, owned reader | runs to its end (blob stored, document put) |
| `AsyncDb::refresh()` | async | runs to its end (the new state, or the old one on an error) |
| `AsyncDb::backup(impl AsRef<Path>)` | async | runs to its end (the backup is complete, or cleaned up on an error) |
| `AsyncCollection::put_json(impl Into<String>)` | async; the JSON is read on the pool | runs to its end |
| `AsyncCollection::get_json(impl Into<String>)` | async, a read | token set; checked before the read |
| `AsyncCollection::put_struct<T: Serialize + ?Sized>(&T)` | async; the value is written as JSON on the caller's thread, then put as `put_json` | runs to its end |
| `AsyncCollection::get_struct<T: DeserializeOwned + Send + 'static>(impl Into<String>)` | async, a read; the `T` is built on the pool | token set; checked before the read |
| `AsyncCollection::search_json(impl Into<String>)` | async, a read | token set; the search stops soon after (SPEC §7.3) |
| `AsyncDb::path()`, `is_read_only()`, `block_cache_stats()` | synchronous on `AsyncDb`: they read the open's state (no I/O; `block_cache_stats` reads counters under the cache's mutex) | (no job) |
| `AsyncCollection::name()`, `schema()`, `durability()`, `with_durability(Durability)`, `to_struct(&Document)` | synchronous on `AsyncCollection`: a name, a lookup in the published state, a handle builder, a conversion of a document at hand; none does I/O of its own (item 14) | (no job) |
| `Db::open`, `OpenOptions::open`, `Db::close` | `AsyncDb::open`, `open_with`, `close` (item 16) | as item 16 |
| the hidden test hooks of `Db` and `Collection` (`maintain_now`, `committed_seq`, `with_writer`, `is_mapped`, `is_pack_mapped`, `hold_writer_while`) | not offered: test hooks, not API | |

Why:

1. **Uploads take an owned reader.** A blob upload streams from a reader, and a job on a pool thread cannot borrow
   the caller's: the reader is moved into the job, so it is `Read + Send + 'static` (a `File`, a `Cursor` over owned
   bytes, a `&'static [u8]`, a channel's reader). `Blob<'r>` holds a `Box<dyn Read + 'r>` that need not be `Send`, so
   the async `put_with_blob` takes the reader and its `PutBlobOptions` instead of a `Blob`; making `Blob`'s reader
   `Send` would refuse a synchronous caller's non-`Send` reader. Bytes in a file are passed as the opened `File`; a
   blob already stored is put by giving the document its reference (`Document::set_blob_ref`) and calling `put`. The
   reader must not call back into the database, as for the synchronous form; on a pool of one thread such a call
   waits behind the upload for ever.
2. **An upload that has started is not cancelled.** Stopping it halfway would leave staged chunks waiting for a resume
   or a GC (ADR-0034), and the bytes the reader yielded would be lost to the caller. So it runs to its end, as a write
   does (item 13): the blob is durable and whole; a `put_blob` whose result is discarded leaves a blob no document
   references, which a later blob GC collects, exactly as a synchronous `put_blob` whose reference is dropped.
   `put_with_blob` stores the blob and puts the document. Cooperative cancellation of an upload is not offered: it
   would need a way for the blob store to stop between chunks and drop what it staged, which it does not have.
3. **No async `get_blob`.** It returns a `BlobReader`, whose reads are I/O on the caller's thread: an async form would
   resolve to a handle that then blocks the caller's executor on every read. An async program reads a blob by ranges
   with `get_blob_range`, each a job on the pool. (Between two range reads a blob that no document references can be
   collected; a `BlobReader` pins it. A program that streams a whole blob from async code can move a synchronous
   `Db`'s `BlobReader` to a thread of its own.) `get_blob` does not write into a caller's writer, so there is no
   borrowed writer to decide about.
4. **Typed values.** `put_struct` writes the value as JSON on the caller's thread when it is called, so it borrows the
   value only for the call and needs no `Send + 'static` bound on `T`; a value that cannot be written is refused at
   once with `InvalidArgument`, and nothing is queued. The JSON is read against the schema on the pool, as
   `put_json` does. `get_struct` builds its `T` on the pool, so `T: DeserializeOwned + Send + 'static`.
5. **`backup` runs to its end.** A backup is a copy of two files (SPEC §4.8) with no point to stop at that would leave
   nothing: `write_backup` has no cancellation, and its clean-up runs only on an error. So a dropped future does not
   stop a long copy; the backup ends complete at `dest`, or is cleaned up as on any error. A caller that wants no
   backup drops the future before the job starts, or removes `dest` after.
6. **`refresh`** reopens and swaps in one state under its lock: no halfway state, so it runs to its end.
7. **`collection_names` stays synchronous**, as `collection` does (item 16): a lookup in the published state, which
   does no I/O of its own, so a job would only add a thread hop (item 14 on the lock it reads under).
8. **Reads** (`get_json`, `get_struct`, `get_blob_range`, `search_json`) run under a token, as `get` and `search` do:
   a dropped future sets it; a lookup checks it before it reads (it has no point to stop at inside), and
   `search_json` passes it to the search, which stops soon after.
9. **Tested** in `crates/persia/tests/async_calls.rs` through the test `block_on`, on a pool of one thread held by the
   test hook: each call against its synchronous result and its job kind; each dropped before it starts (skipped,
   nothing left: no collection, document, blob or backup file); each change dropped after it starts (it ran whole);
   each read dropped after it starts (its token set, `Cancelled`); jobs queued at `close` (`Closed`); every owned
   reader dropped once its job is gone; and every test closes and reopens the file.
10. **No Go, TypeScript or C form yet.** The embedded bindings of ADR-0018 do not expose the async API, so these
    calls have no form there.
11. **`backup`'s `dest` is read at the call** (review): parsed as the synchronous backup parses it (a `file://` URL
    read, an empty or malformed one `InvalidArgument`, another scheme `Unsupported`), refused at once with nothing
    queued, and a relative path made absolute against the working directory at the call, so a later change of it does
    not move the backup. Whether `dest` is free is still checked by the job, when it runs.
12. **A skipped upload keeps its reader until the pool reaches it** (review): dropping a queued upload's future marks
    it skipped, but the job, and the reader moved into it, stay in the queue until a pool thread takes it or the close
    (or the drop of the `AsyncDb`) completes the queue. A reader that holds a file or a socket keeps it until then.
13. **The synchronous calls on the async handles** (review): every call of `Db` and `Collection` that does
    no I/O of its own has a synchronous form on `AsyncDb` or `AsyncCollection` (the rows above), so an async program
    needs no `Db` beside its `AsyncDb`. Only `get_blob` (item 3) and the hidden test hooks have none.
14. **"No lock" means no writer mutex** (re-review). `collection`, `collection_names`, `AsyncCollection::schema` and
    `to_struct` read the published state through the read side of its lock, and `block_cache_stats` reads counters
    under the cache's mutex. None does I/O of its own and none waits for the writer. On a read-write database the
    write side of the state lock is taken only at close. On a read-only database a running `refresh` holds it while it
    refreshes the pack, so these calls can wait for that I/O. Item 16 above says `collection` "takes no lock": read it
    with this correction. Moving the pack refresh out from under the write side is not part of this change.

## Amendment (2026-11-27): `JobFuture`'s auto traits in public terms (ROADMAP 10.1.36)

The public-API snapshot (`crates/persia/public-api.txt`, ADR-0063) listed `JobFuture<T>`'s auto-trait impls as
`where alloc::rcs::arc::Arc<Slot<T>>: Send` (and `Sync`, `Unpin`), naming the facade's private `Slot<T>`. The rustdoc
of the dated nightly writes a synthesized auto-trait impl's bounds as the types of the fields that carry the type
parameters, without reducing them to the parameters (a scratch crate shows it for any field: `Mutex<Option<T>>`,
`PhantomData<T>` and a private wrapper are written as they are; only a field of type `T` reads `T: Send`).

**What the conditions are** (unchanged by this amendment, derived from the fields): `JobFuture<T>` holds
`Arc<Slot<T>>`, `Option<JoinHandle<()>>` and a `bool`; the last two are `Send`, `Sync` and `Unpin` for every `T`.
`Arc<X>` is `Send` and `Sync` exactly when `X: Send + Sync`. `Slot<T>` holds `Mutex<State<T>>` and
`Option<CancelToken>` (`Arc<AtomicBool>`: both always). `Mutex<S>` is `Send` when `S: Send` and `Sync` when `S: Send`;
`State<T>` holds a `Phase`, an `Option<Waker>` and an `Option<Result<T, persia::Error>>`, whose error is `Send + Sync`,
so `State<T>: Send` exactly when `T: Send`. Therefore:

- `JobFuture<T>: Send` exactly when `T: Send`;
- `JobFuture<T>: Sync` exactly when `T: Send` (not `T: Sync`: the result crosses threads through the mutex, and a
  shared `&JobFuture` gives no access to it);
- `JobFuture<T>: Unpin` for every `T` (`Arc` is always `Unpin`; `poll` already relies on it through `Pin::get_mut`);
- `UnwindSafe` and `RefUnwindSafe`: never, as before (the snapshot's `!` lines are unchanged).

**Decision.** `Unpin` is stated in code, `impl<T> Unpin for JobFuture<T> {}`: a safe trait, so a plain impl, exactly
today's condition, and the snapshot reads `impl<T> core::marker::Unpin for persia::JobFuture<T>`. `Send` and `Sync` are
unsafe traits: an explicit impl needs `unsafe`, which this crate forbids (CLAUDE.md), so they stay derived by the
compiler, and the snapshot's renderer states their bounds: `STATED_AUTO_TRAIT_BOUNDS` in
`xtask/src/public_api/render.rs` holds `(persia::JobFuture<T>, Send, T: core::marker::Send)` and the same for `Sync`,
and a synthesized auto-trait impl whose bounds name a type rustdoc's `paths` does not list is rendered with its entry,
or fails the rendering without one (so a new private name in such bounds cannot reach the snapshot unnoticed). The
entries are not computed by the renderer; what keeps them true is `crates/persia/tests/job_future_auto_traits.rs`:
generic functions the compiler checks for every `T: Send` (`Send` and `Sync`) and every `T` (`Unpin`), and
compile-time probes (an inherent associated constant behind a `Send` or `Sync` bound, with a trait's constant of the
same name standing in when the bound does not hold, checked against `u8`, `Rc`, `Cell` and `MutexGuard` first) that
`JobFuture<Rc<u8>>` and `JobFuture<MutexGuard<'static, u8>>` are neither `Send` nor `Sync` and `JobFuture<Cell<u8>>`
is both. A change of the fields that changes a condition fails that test to compile, and the entry is updated with it.

**Alternatives turned down.** Restructuring the fields so rustdoc writes public bounds: only a field of type `T` does,
which a shared slot cannot be; a field of public std types (`Arc<Mutex<(..)>>`) would still be written whole, would
need the slot's phase and waker in std types too, and would bend the code to the renderer. A type-erased slot
(`Arc<dyn Any + Send + Sync>` and a marker) needs `T: 'static` and a downcast at every poll. Renderer rules that compute
auto traits would reimplement the trait solver.

Snapshot change: the three lines `where alloc::rcs::arc::Arc<Slot<T>>: ...` became
`impl<T> core::marker::Send for persia::JobFuture<T> where T: core::marker::Send`, the same for `Sync`, and
`impl<T> core::marker::Unpin for persia::JobFuture<T>`. The snapshot test now checks that no line names `Slot<` and
that the three lines are listed.
