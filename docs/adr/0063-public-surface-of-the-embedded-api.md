# ADR-0063: The public surface of the embedded API: facade types, a fixed re-export list, one opaque error with stable codes

- Status: proposed
- Date: 2026-11-23
- Related: SPEC §8.1, §9.6, §13, §14; ROADMAP 10.1.1–10.1.5, 10.1.9, 10.1.12, 10.5.1; ADR-0018, ADR-0051;
  `docs/sdk/c-abi.md`

## Context

`persia` was a stub over a hidden `internal` layer (ADR-0051). M10 makes it the crate users import, and the Go and
TypeScript bindings wrap exactly what it exposes (ADR-0018). The engine's types were written for the engine: a
`Database` whose writes take `&mut self`, options structs with public fields, twenty-odd error enums. Exposing them
would freeze the engine's shape. The owner decided the surface on 2026-11-23 (wave 7, decisions a and c).

## Decision

1. **Facade types for everything with behaviour.** Handles, options, requests, responses and errors are types of
   `persia`: `Db`, `OpenOptions`, `Collection`, `Blob`, `BlobReader`, `PutBlobOptions`, `BlockCacheStats`, `Error`,
   `ErrorCode`, and the request API (ROADMAP 10.1.3): `SearchRequest` (a builder, lowered to the engine's
   `QueryRequest`, ADR-0062), `SearchResponse`, `Hit` and `Relaxation`; a cursor crosses it as an opaque token
   (`String`). `Db`
   is `Send + Sync`, not `Clone`, with every method on `&self`. A `Collection` owns an `Arc` of the database (no
   lifetime), and its calls fail with `Closed` once the database is closed.
2. **A fixed allow-list of engine value types**, re-exported under `persia::` because they are plain values with no
   handle behind them: `Document`, `Value`, `CollectionSchema` and its field types (`SchemaBuilder`, `Field`,
   `FieldId`, `FieldKind`, `FieldType`, `TextOptions`, `KeywordOptions`, `AnalyzerRef`, `RankingConfig`,
   `RelaxationConfig` and the types those name), `Durability`, `BlobRef`, `BlobId`, and for search: `RelaxOptions`,
   `RelaxLevel`, `Filter` and `FilterValue`, `Sort` with `SortKey`, `Order` and `Missing`, `FacetRequest`,
   `FacetCounts` and `FacetValue`, `Explanation` and the explanation types its variants hold. Adding to the list is
   a decision recorded here.
3. **Never public:** `Database`, `DatabaseOptions`, `Snapshot`, `Segment`, `Container`, `Manifest`, `SearchPlan`,
   `Execution`, the query AST, and any error type of the engine, the blob store or the storage layer. No public
   signature of a facade type names one. The test hooks (`Db::maintain_now`, `committed_seq`, `is_mapped`,
   `OpenOptions::{clock, hold_maintenance, drop_wait, detached_into}`, `DetachedSlot`,
   `Collection::hold_writer_while`) exist only with the non-default `test-hooks` feature, which the crate's own tests
   turn on through a dev-dependency on itself; the hidden `persia::internal` layer (ADR-0051) is public only with the
   non-default `internal` feature, which `persia-cli` and the fuzz targets enable. The default build exposes neither,
   nor any engine handle or error type.
4. **The re-exported types keep their own methods**, which are part of the surface whether or not `persia`
   re-exports what they mention: `CollectionSchema::from_json` returns the engine's `SchemaError`, `Document::get`
   takes a `FieldId`. A caller can use such a value (`?` into `Box<dyn Error>`, print it) without naming its type.
   The facade offers a `persia::Result` path beside each one a caller needs (`Db::create_collection_json`,
   `Collection::put_json`, `Collection::get_json`), so a program can stay on one error type.
5. **`Document::set_blob_ref` is one of those methods.** SPEC §9.6 forbids a dangling reference, not the method:
   every `put` of a document that carries a reference pins the blob and checks that this database's store holds it
   before the document is logged, exactly as `put(doc, Blob::Stored(reference))` does, and fails with `NotFound`
   otherwise. A reference set by hand to a blob the database stored is the same as one `put_blob` returned.
6. **One opaque error.** `persia::Error` has `code() -> ErrorCode`, `message()` (also its `Display`), and the lower
   layer's error as `std::error::Error::source()`. It has no public constructor and no public variant. `Result<T>` is
   the alias.
7. **Stable codes.** `ErrorCode` is a `#[non_exhaustive]` enum whose `number()` and `as_str()` are the numbers and
   names of the C ABI's table (`docs/sdk/c-abi.md`): 1 to 12 as designed there, 6 widened to a URL scheme this build
   has no backend for (every scheme but `file://`, owner decision b: an unknown scheme is not
   malformed input), and three appended: 13 `PERSIA_READ_ONLY`, 14 `PERSIA_POISONED` (a failed write stopped the
   writer; reopen), 15 `PERSIA_CANCELLED` (deadline or cancel). 9 `BUSY` and 10 `PANIC` are reserved for the ABI and
   never returned from Rust. The list is append-only.
8. **The mapping is one module**, `crates/persia/src/error/classify.rs`: one exhaustive `match` per engine error enum,
   so a new variant does not compile until it has a code. Choices that are not obvious:
   - A missing document is `Ok(None)` from `get` in Rust; the ABI turns it into `NOT_FOUND`.
   - Limits of SPEC §17 are `LIMIT` (a document, id, keyword or term too long, too many fields, values, terms,
     clauses, expansions, sort keys or facets, the result window); an empty id is `INVALID_ARGUMENT`.
   - An analyzer the open lacks (`UnregisteredAnalyzer`) is `UNSUPPORTED`: SPEC §6.2.2 cannot tell a newer build's
     built-in from one the caller did not register.
   - A refused refresh of a read-only handle (another database or an older state at the path) is `CONFLICT`.
   - A bind whose outcome is unknown is `TRANSIENT` (binding again is safe: blobs are content-addressed), or
     `POISONED` when the pack stopped writing. A contended reader lease is `TRANSIENT` (open again).
   - **Fallbacks.** A failure that fits no code, an allocation that failed or an internal invariant that did not
     hold (`DatabaseError::Internal`, `ApplyError`), is `IO`. The engine's `#[non_exhaustive]` enums are all errors of
     a search request: a variant this build does not know is `INVALID_ARGUMENT`.
9. **Storage and format errors are classified by kind.** `persia` may not depend on `persia-storage` or
   `persia-format` (`cargo xtask check-deps`). Each of their errors has a `kind()`: `persia_format::ErrorKind`,
   `persia_storage::container::ErrorKind` and `persia_storage::ErrorKind`, plain enums (not `#[non_exhaustive]`) that
   the engine re-exports as `persia_engine::error_kinds`. `classify.rs` maps them with exhaustive matches, so a class
   added or renamed below fails to compile there, and a table test lists every class with its code. Nothing is read
   from an error's `Debug` or `Display` text. (Until 2026-11-24 the variant's `Debug` name was read; replaced in
   review.)
10. **The surface is guarded by a committed snapshot** (ROADMAP 10.1.12), recorded with that leaf. `cargo semver-checks`
    waits for the first publish (ROADMAP 10.5.1).

## Consequences

- The engine can change `Database`, its options and its errors without a breaking change of `persia`. It cannot
  change the re-exported value types without one: the allow-list is where the engine's shape is frozen.
- A caller branches on fifteen codes, never on an engine error. Detail the codes lose (which document of a batch
  was rejected, the resume token of an interrupted upload) is in the message and behind `source()` only.
- Item 9 adds a small, stable classification API to `persia-format` and `persia-storage` (one `kind()` each), and
  one re-export module to the engine, instead of re-exporting their whole error types.
- `IO` covers "none of the above". A caller cannot tell a bug from a failed disk by the code; a dedicated code
  (`16 PERSIA_INTERNAL`) can be appended if that matters.
- **What the re-exports freeze.** Item 4 makes every public method and field of the re-exported types part of
  `persia`'s surface, including some that are engine detail: `CollectionSchema::hash`, `hash_at` and
  `canonical_index_definition`; `CollectionSchema::from_json_in` and `AnalyzerRef::parse_in`, which name
  `persia_analysis::AnalyzerRegistry`; `Document::encode`, `decode` and `encoded_id` (the binary encoding of SPEC
  §6.4.2); `Document::extra` and `insert_extra`, which expose `serde_json::Value`; and the schema types that are not
  `#[non_exhaustive]` (`TextOptions`, `KeywordOptions`, `RankingConfig`, `RelaxationConfig`, `FieldKind`, `FieldType`,
  `Level`, `Fallback`, `FilterRelaxation`, `RankFn`, `StaticRank`), whose public fields and variants cannot grow
  without a breaking change. Marking them `#[non_exhaustive]` and trimming that surface is ROADMAP 10.1.16, due
  before the first publish (10.5.1).
- **A classification rule beyond the tables:** corruption wins. An engine or documents error that reports
  corruption anywhere (`is_corruption()`, also in the later step of a `ThenFailed`) is `CORRUPT`, whatever came
  first: a damaged file is never reported as the lesser failure in front of it.

## Alternatives considered

- **Re-export the engine's `Database` behind a thin wrapper.** Less code, and every engine refactor becomes a
  breaking change of the product's API.
- **A public error enum with data** (`Error::NotFound { collection }`). Each variant's fields become API, and the
  bindings can carry only a code and a message anyway.
- **`From<DatabaseError> for persia::Error` as public impls.** They would put the engine's error types in the
  public surface; the conversions are crate-private functions instead.
- **Wrapper types for `Document` and `CollectionSchema`.** A parallel value model to keep in step with the engine's,
  for types that are already plain data with a documented JSON form (SPEC §6.2, §6.4.1).

## Amendment (2026-11-26): the facade's own blob options (ROADMAP 10.1.15)

`PutBlobOptions` was the `internal` layer's struct, re-exported, whose public fields named two `persia-blob` types
that are on no list here (`ChunkSize`, `ResumeToken`): a caller could not set `chunk_size` or `resume` without
depending on `persia-blob`. The owner chose a facade type over adding the two to item 2:

- `persia::PutBlobOptions` is the facade's own `#[non_exhaustive]` struct with private fields: `new()` (also
  `Default`) and one consuming setter per option, `mime`, `user_meta(key, value)` (adds one entry), `expected_id`,
  `expected_size`, `chunk_size(u32)` (bytes), `bind_budget(Duration)` and `resume(ResumeToken)`. It converts to the
  blob layer's options inside the facade, when the blob is stored; a chunk size outside 64 KiB..=64 MiB is then
  `INVALID_ARGUMENT`, and the blob layer checks the rest as before.
- `persia::ResumeToken` is the facade's value of SPEC §9.4's token, the upload id (`u128`) and the chunk size
  (`u32`): `new(upload_id, chunk_size)`, `upload_id()`, `chunk_size()`, so a caller can keep it across a restart.
  `Error::resume_token()` finds it behind an interrupted upload's error (a downcast of the source chain, never its
  text). This narrows the first Consequence above: the resume token is no longer reachable only through `source()`.
- `persia::Blob` is the facade's own enum too (the same variants, `Reader`, `Path` and `Stored`, holding the facade's
  options), since its `with_options` takes them; it converts to the `internal` layer's `Blob` the same way.

No public signature of `persia` names a `persia-blob` type now other than `BlobRef` and `BlobId` (item 2).

## Amendment (2026-11-26): `#[non_exhaustive]` and the trim of the re-exported engine types (ROADMAP 10.1.16)

The Consequence "What the re-exports freeze" listed what item 4 made part of `persia`'s surface. Each item is now
hidden, wrapped or kept, before the first publish (ROADMAP 10.5.1):

| Item | Decision |
|---|---|
| `TextOptions`, `KeywordOptions`, `RankingConfig`, `RelaxationConfig`, `StaticRank` | **`#[non_exhaustive]`.** The public fields stay (read them, or set them on a value you have), so a field can be added; another crate cannot write a struct literal, so each gets one consuming setter per field (`TextOptions::default().boost(3.0).positions(true)`, `StaticRank::new("sales").weight(0.5)`, `RelaxationConfig::default().min_results(5)`, ...). |
| `FieldKind`, `FieldType`, `Level`, `Fallback`, `FilterRelaxation`, `RankFn` | **`#[non_exhaustive]`**: a variant can be added; a match outside the engine needs a wildcard arm. The fields of `FieldKind`'s struct variants (`I64 { stored }`, ...) are **kept** exhaustive on purpose: they are the field's options, and a new option of those types would be a new variant holding an options struct, as `Text` and `Keyword` do. |
| `CollectionSchema::hash`, `hash_at`, `canonical_index_definition` | **Hidden** (`#[doc(hidden)]` on the engine's methods): the schema hash segments record (SPEC §6.2.5) is format detail, and `hash` named `SchemaHash`, on no list here. The engine, its tests and the CLI still call them. |
| `CollectionSchema::from_json_in`, `AnalyzerRef::parse_in` | **Hidden**: they take `persia_analysis::AnalyzerRegistry`, which `persia` does not export, and the facade has no way to register an analyzer yet. A registry option of `OpenOptions` would bring a facade path back. |
| `Document::encode`, `decode`, `encoded_id` | **Hidden**: the binary encoding of SPEC §6.4.2 is the engine's; a caller has the JSON form (`Collection::get_json`, `Document::to_json`) and the typed values. |
| `Document::extra`, `insert_extra` | **Kept**: the extra fields of a collection that is not `strict` (SPEC §6.4.1) are a user feature, and `serde_json::Value` is how they are typed. `serde_json` 1.x becomes a public dependency of `persia` (as `serde` is through `Collection::put_struct`), so a major version of `serde_json` would be a breaking change of `persia`. |

"Hidden" is `#[doc(hidden)]`, as for `persia::internal` (ADR-0051): the methods stay callable, outside the documented
surface and the public-API snapshot (ROADMAP 10.1.12), and carry no stability promise. In-repo users that need one
keep calling it through the engine, or through `persia::internal`; none moved to a new public item.
