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
