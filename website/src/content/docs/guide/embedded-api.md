---
title: "Embedded API"
description: "The planned Db and Collection API. Method names are illustrative and are not in the crate."
head:
  - tag: title
    content: "Planned embedded Rust API: Db::open and search | PersiaDB"
  - tag: meta
    attrs:
      property: og:title
      content: "Planned embedded Rust API: Db::open and search | PersiaDB"
  - tag: meta
    attrs:
      name: twitter:title
      content: "Planned embedded Rust API: Db::open and search | PersiaDB"
---

**Planned.** The usage walkthrough, including what works today and the FAQ, is [Getting started](/guide/getting-started/).
The public crate `persia` is a stub. %%M10_BLOCKED%% %%ENGINE_CALLS%%
Method names below are illustrative and not final. This is the sketch already in the README:

```rust
let db = persia::Db::open("app.persia")?;
// Collections declare their schema up front (SPEC §6.2).
let products = db.create_collection(schema)?;
products.put(doc)?;
products.commit()?;

// No exact match for the typo? The query is relaxed (here: fuzzy, SPEC §7.6),
// and the response says what changed: "runing" -> "running".
let results = products.search("runing")?;
```

The intended surface (SPEC §8, §9.6, ROADMAP 10.1):

- `Db::open` on a path or URL, `OpenOptions`, and `close`.
- Create, open, and drop a collection, and read its schema back.
- `put`, `put_batch`, `delete`, `get`, `commit`, and `search`.
- Documents as a dynamic map, or as a `serde` struct (`put_struct`).
- `put_blob` / `get_blob` / ranged reads. A convenience `put` that takes a document and a blob writes
  the blob first, then commits the document.
- A stable error type. Library code returns `Result`; corrupted bytes are an error, never a panic.

Handles are `Send` and `Sync`. Many readers share a snapshot. A second writer fails immediately.
A `put` is visible to later searches on that database (read-your-writes, SPEC §8.2–§8.4).
An optional `tokio` feature will offer a thin `spawn_blocking` wrapper (ROADMAP 10.1.6); the core
API stays synchronous.

Embedded mode has no authentication and no config file. Knobs, once they exist, are builder options
with defaults: memtable size, commit interval, fsync policy, merge threads, search threads, cache
sizes, block size, blob chunk size, and the `relax.*` settings (SPEC §14).

The `persia` command-line tool's later commands (`compact`, `stats`, JSONL import,
a one-shot search, and a small bench) are part of the same milestone (ROADMAP 10.4) and are not in the binary.
`persia inspect segment` and `persia dump segment` are, and `--verify` checks the segment. File-sweep commands exist too.
None of that is `Db::open`. The `v0.1.0-alpha` tag is specified to say, in the release itself, that
the format is still unstable (ROADMAP 10.5.2).
