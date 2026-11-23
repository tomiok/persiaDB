# ADR-0064: The public surface of the embedded API: facade types, a fixed re-export list, one opaque error with stable codes

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
   `persia`: `Db`, `OpenOptions`, `Collection`, `Blob`, `BlobReader`, `PutBlobOptions`, `Error`, `ErrorCode`, and,
   once the request API lands (ROADMAP 10.1.3's `search`), `SearchRequest`, `SearchResponse` and the hit types. `Db`
   is `Send + Sync`, not `Clone`, with every method on `&self`. A `Collection` owns an `Arc` of the database (no
   lifetime), and its calls fail with `Closed` once the database is closed.
2. **A fixed allow-list of engine value types**, re-exported under `persia::` because they are plain values with no
   handle behind them: `Document`, `Value`, `CollectionSchema` and its field types (`SchemaBuilder`, `Field`,
   `FieldId`, `FieldKind`, `FieldType`, `TextOptions`, `KeywordOptions`, `AnalyzerRef`, `RankingConfig`,
   `RelaxationConfig` and the types those name), `Durability`, `BlobRef`, `BlobId`, and with `search`: `RelaxOptions`,
   `Filter`, `Sort`, the facet request and counts, `Explanation`. Adding to the list is a decision recorded here.
3. **Never public:** `Database`, `DatabaseOptions`, `Snapshot`, `Segment`, `Container`, `Manifest`, `SearchPlan`,
   `Execution`, the query AST, and any error type of the engine, the blob store or the storage layer. No public
   signature of a facade type names one. (Test hooks are `#[doc(hidden)]`, are not API, and may.)
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
   has no backend for, and three appended: 13 `PERSIA_READ_ONLY`, 14 `PERSIA_POISONED` (a failed write stopped the
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
9. **Storage errors are classified by name.** `persia` may not depend on `persia-storage` or `persia-format`
   (`cargo xtask check-deps`), and the engine re-exports none of their error types, so `persia` cannot match on the
   container's error or on the storage error kind. It uses the methods they have (`is_corruption`, `kind`) and, where
   none tells (`Locked`, `ReadOnly`, `Poisoned`, the kinds), the variant's name as `Debug` prints it. The table test
   names those types through `persia`'s dev-dependency on `persia-storage` and matches on them exhaustively, so a
   variant added or renamed fails that test. An unknown name is `IO`.
10. **The surface is guarded by a committed snapshot** (ROADMAP 10.1.12), recorded with that leaf. `cargo semver-checks`
    waits for the first publish (ROADMAP 10.5.1).

## Consequences

- The engine can change `Database`, its options and its errors without a breaking change of `persia`. It cannot
  change the re-exported value types without one: the allow-list is where the engine's shape is frozen.
- A caller branches on fifteen codes, never on an engine error. Detail the codes lose (which document of a batch
  was rejected, the resume token of an interrupted upload) is in the message and behind `source()` only.
- Item 9 is a workaround. A re-export of the storage error types from `persia-engine` (as it re-exports
  `persia_analysis`) would let `classify.rs` match on variants; the table test would stay as it is.
- `IO` covers "none of the above". A caller cannot tell a bug from a failed disk by the code; a dedicated code
  (`16 PERSIA_INTERNAL`) can be appended if that matters.

## Alternatives considered

- **Re-export the engine's `Database` behind a thin wrapper.** Less code, and every engine refactor becomes a
  breaking change of the product's API.
- **A public error enum with data** (`Error::NotFound { collection }`). Each variant's fields become API, and the
  bindings can carry only a code and a message anyway.
- **`From<DatabaseError> for persia::Error` as public impls.** They would put the engine's error types in the
  public surface; the conversions are crate-private functions instead.
- **Wrapper types for `Document` and `CollectionSchema`.** A parallel value model to keep in step with the engine's,
  for types that are already plain data with a documented JSON form (SPEC §6.2, §6.4.1).
