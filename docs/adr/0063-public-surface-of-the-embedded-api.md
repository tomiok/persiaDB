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

## Amendment (2026-11-26): the public-API snapshot is `cargo xtask public-api` (ROADMAP 10.1.12)

Item 10 left the snapshot to ROADMAP 10.1.12, and Step 0 decision (a) of wave 7 had named a `cargo public-api`
snapshot. The owner replaced it (2026-11-25): the repository's own `cargo xtask public-api`, with no `cargo-public-api`
to install, in the repo tooling that already reads JSON (`serde_json`, allowed in `xtask`).

1. **What it reads.** rustdoc's JSON (an unstable output, built with the dated nightly of `.github/nightly-toolchain`,
   as Miri and the fuzz targets are) for `persia` and for the workspace crates it re-exports from (`persia-engine`,
   `persia-blob`, `persia-format`, `persia-analysis`), into `target/public-api`. The renderer reads one format version
   (61, the pinned nightly's) and refuses another, so bumping the nightly is where it is updated.
2. **Re-exports expanded.** rustdoc's JSON keeps a re-export of another crate's item as a `use`, even with
   `#[doc(inline)]` (which `persia`'s re-exports now carry, for its HTML docs). The renderer follows each one into the
   defining crate's JSON and lists the item's fields, variants, methods and trait impls under the path a user writes
   (`persia::TextOptions::boost`); signatures name a type by that path when the surface reaches it, else by its
   canonical path (`persia_engine::schema::SchemaError`, `core::option::Option`).
3. **What it lists.** One sorted line per public item, field, variant, method, associated item and trait impl, with
   `#[non_exhaustive]`; auto-trait impls only for `Send`, `Sync`, `Unpin`, `UnwindSafe` and `RefUnwindSafe`; no
   blanket impls, no `StructuralPartialEq`, no docs. `#[doc(hidden)]` items are not in the JSON, so the hidden engine
   detail of the 10.1.16 amendment is not listed.
4. **Which features.** The ones a user can enable and rely on: the default and `async`. Not `internal` (the hidden
   layer, ADR-0051) or `test-hooks`, which are not stable.
5. **The gate.** The listing is committed as `crates/persia/public-api.txt`; `just public-api` regenerates it, and
   `cargo xtask public-api --check` (CI's `repo-checks` job, and `just repo-checks`) fails with the lines added and
   removed when the surface moved without the file.
6. **Generated on Linux.** rustdoc renders the target it runs on, so the committed file is the surface on Linux
   (x86_64): CI's `repo-checks` job runs on `ubuntu-latest`. Items behind `cfg(windows)` or another target are not in
   it; none of `persia`'s public items is target-specific today.
7. **No rustdoc warning** (ROADMAP 10.1.49). The build of item 1 runs rustdoc with `-D warnings`, so a rustdoc
   warning in `persia` or a crate it re-exports from (`persia-engine`, `persia-blob`, `persia-format`,
   `persia-analysis`), such as a broken or private intra-doc link, fails `cargo xtask public-api` and its `--check`
   in CI. Here rather than a `cargo doc` line in the lint job: it is the one CI step that already runs rustdoc on
   those crates, so the gate costs no second documentation build, and these are the crates whose docs a `persia`
   user reads (re-exported items carry their defining crate's docs). The crates `persia` does not re-export from
   (`persia-storage`, `persia-cli`, `persia-server`, `persia-proto`, `persia-docs`, `persia-testutil`, `xtask`: every
   crate other than `persia` and the four above) are not gated.

## Amendment (2026-11-26): `Correction` and `FallbackOrder` join item 2 (ROADMAP 10.1.25, review)

`persia::Relaxation` has typed getters for what the response JSON states beside its public fields: `corrected()`
(`&[Correction]`), `fallback()` (`Option<FallbackOrder>`) and `filters_dropped()`. The two engine types they name are
plain values of the relaxation cascade, re-exported under `persia::` beside `RelaxLevel`; `Correction` is now
`#[non_exhaustive]`, so its fields can grow. `persia::Blob` is `#[non_exhaustive]` too.

## Amendment (2026-11-27): the rest of the engine and format detail on the re-exported types (ROADMAP 10.1.29)

The public-API snapshot (`crates/persia/public-api.txt`) showed engine and format detail beyond the 10.1.16 list. Each
item is now hidden, wrapped or kept; "hidden" is `#[doc(hidden)]` as in the 10.1.16 amendment (callable, outside the
documented surface and the snapshot, no stability promise):

| Item | Decision |
|---|---|
| `BlobRef::new`, which returns `persia_format::error::EncodeError` | **Hidden and wrapped.** Callers do build references by hand (a reference kept outside the database, item 5; the mismatched references of ROADMAP 10.1.33), so hiding it alone would leave no typed way to build one. The facade's wrap is the free function `persia::blob_ref(blob_id, size, mime, chunk_size, user_meta) -> persia::Result<BlobRef>`, with the same value rules (SPEC §9.1) and `INVALID_ARGUMENT` when one does not hold (the format layer's error is its `source()`). A free function, not a method: `BlobRef` is another crate's type, so `persia` cannot add an inherent constructor, and a facade wrapper type of `BlobRef` would be the parallel value model this ADR's alternatives turned down. Keeping `new` would have left a format-layer error type in a public signature, which item 3 rules out for every type but the re-exported values' own methods of item 4. |
| `BlobRef::encode`, `decode`, `encoded_len` (naming `persia_format::writer::Writer`, `reader::Reader` and `error::Result`) | **Hidden**: the binary encoding of SPEC §9.1 is the format layer's, as `Document::encode` is the engine's (10.1.16). A caller has the getters and the reference's JSON form in a document (`blob`, SPEC §6.4.1). |
| `BlobRef::VERSION`, `MAX_ENCODED_LEN` | **Hidden** with the encoding they describe. The value-rule constants `MAX_CHUNK_SIZE`, `MAX_MIME_LEN`, `MAX_META_ENTRIES`, `MAX_META_KEY_LEN` and `MAX_META_VALUE_LEN` are **kept**: they are the limits `persia::blob_ref` checks. |
| `Filter::from_json`, `Filter::validate`, which take `persia_engine::query::limits::QueryLimits` (and return `QueryError`) | **Hidden**: `persia` does not export `QueryLimits`, so a program that depends on `persia` alone could not call them. A search checks its request's filters (`Collection::search`), and `Collection::search_json` reads them from a request's JSON. If a caller needs a filter parsed from JSON on its own, a facade method that applies the default limits can be added. |
| The engine error types the re-exported values' methods return: `SchemaError` (`CollectionSchema::from_json`, `to_json`, `SchemaBuilder::build`), `EvolutionError` (`CollectionSchema::evolve`), `DocumentError` (`Document::from_json`, `to_json`, `validate`), `RequestError` (`FacetRequest::new`, `Sort::new`), `RelaxError` (`RelaxOptions::from_json`, `validate`) | **Kept**, as item 4 decided: they are the values' own methods, named by their canonical path and not re-exported; a caller uses them through `?` into `Box<dyn Error>` and prints them, and the facade offers a `persia::Result` path beside each one a caller needs (`Db::create_collection_json`, `Collection::put_json`, `get_json`, `search_json`). |
| `persia_format::blob::ParseBlobIdError`, the `Err` of `BlobId`'s `FromStr` | **Kept**, as item 4's errors are. The snapshot does not show it: the renderer lists a trait impl, not its associated types. |

No public signature in the snapshot names a `persia_format` type now; the engine types it names are the errors of
the last two rows. The snapshot test (`the_committed_snapshot_lists_only_what_is_kept_on_purpose` in
`xtask/src/public_api/tests.rs`) checks that none of the hidden items, `persia_format::` or `QueryLimits` is listed,
and that `persia::blob_ref` is.

## Amendment (2026-11-27): a filter in its JSON form on `SearchRequest` (ROADMAP 10.1.35)

With `Filter::from_json` hidden (10.1.29: it takes the engine's `QueryLimits`), a caller could send a filter's JSON
only inside a whole request JSON (`Collection::search_json`). The facade now takes it on the request builder:
`SearchRequest::filter_json(impl Into<String>)` and `soft_filter_json(impl Into<String>)` hold the text of one SPEC
§7.1.2 object, and the search parses it when it runs (SPEC §7.1.4), with the parser `search_json` reads `filter` and
`soft_filter` with (`Filter::from_json`, which the `filter_json` fuzz target covers), against the schema of the
snapshot the search runs on and under the database's query limits. Those are SPEC §7.1.3's defaults, which the facade
applies itself, in one place (`query_limits` in `crates/persia/src/search.rs`) that the request-JSON path, the filter
JSON and the run all take; `QueryLimits` stays unexported. Each setter takes its request's one hard or soft filter, as
`filter` and `soft_filter` do: the later call is the one kept.

**Why a setter, and not a facade parser that returns a `Filter`** (the other form 10.1.29 named): the request builder
is the facade's one way to state a search, it holds no schema, and what it takes by name is resolved when it runs
(`fields`), so a filter by name fits there and nowhere new is needed: no method on `Collection` or `AsyncCollection`,
no async form, no schema to pass. It parses the text where `search_json` does, on the same snapshot, so the two forms
cannot drift apart: the same filter selects the same documents, ranks them alike and pages with the same cursors
(the cursor's request fingerprint, ROADMAP 10.1.24, hashes the parsed filter). A parser on `Collection` would check a
filter against the schema of the moment it is called, which the search checks again on its own snapshot, and would
need an async twin. What the setter does not give is a typed `Filter` to combine with one built in code (an `and` of
the two); a caller writes the whole filter in one of the two forms. If that need appears, a parser can be added beside
the setter without changing it.

**Errors** are the search's, by `ErrorCode` (item 8): text that is not a filter (malformed, a repeated key, an unknown
operator or field, a value of another type, longer than 1 MiB) is `INVALID_ARGUMENT`; a filter past `max_terms`,
`max_clauses` or `max_depth` is `LIMIT`, as it is inside a request JSON and as every query past a SPEC §7.1.3 limit
is. (ROADMAP 10.1.35's text says `INVALID_ARGUMENT` for a filter past the limits; that would make the two forms of
one filter fail with two codes and contradict item 8, so it is `LIMIT`.) The message names the setter
(`filter_json: ...`). Tests: `crates/persia/tests/filter_json.rs` (seeded cases: the response JSON of a filter given
to the setter equals the one of the same filter inside a request JSON, or both fail with one code; one test per
limit; refused text; the later setter kept; a cursor of one form pages the other).

## Amendment (2026-11-27): the website guides are checked against the public surface, outside `persia` (ROADMAP 10.1.34, 10.1.41, 10.1.42, 10.1.43, 10.1.53)

ROADMAP 10.1.34 made the Rust samples of two website guides doctests of `persia` (`crates/persia/src/guides.rs`,
`#[doc = include_str!("../../../website/...")]`). A crate's directory is all `cargo package` ships, so those paths
would have failed the first publish of `persia`; its unit test of the C ABI table (`docs/sdk/c-abi.md`) read outside
the crate too.

1. **A crate that is never published holds the checks: `crates/persia-docs`** (`publish = false` written in its
   manifest, not inherited, so it stays when `persia`'s is flipped). Its library is empty but for a
   `#[cfg(doctest)] mod guides;`, one item per page whose doc is the page; its `tests/c_abi_codes.rs` checks the C ABI
   table against `ErrorCode::ALL` (`persia`'s own unit test still pins `ALL` to the append-only snapshot). It has
   `persia` as its one internal dependency, a dev-dependency, with the features a user relies on (the default and
   `async`, item 4 of the snapshot amendment), and `tempfile` for the pages' temporary directories. `ALLOWED` in
   `xtask/src/check_deps.rs` lists it with `persia`; the no-async walk (`EMBEDDED` in
   `xtask/src/check_deps/no_async.rs`) includes it, since a runtime among its dev-dependencies would be one a sample
   could use. Not an xtask step: xtask may depend only on `persia-format`, and rustdoc compiling the samples against
   the real crate is the check.
2. **Which `persia` the samples see.** A doctest gets an `--extern` for each dependency of the crate under test:
   here its dev-dependencies `persia` and `tempfile` (the pages' temporary directories), so `persia_engine::...`
   does not resolve. Built alone (`cargo test -p persia-docs`),
   `persia` has the default and `async` features, and `persia::internal` (private without `internal`) does not
   compile. Inside `cargo test --workspace` cargo's resolver unifies features across every package the invocation
   builds: `persia-cli` turns `internal` on and `persia`'s own tests `test-hooks`, so the one `persia` the doctests
   link has `persia::internal` public (checked: a sample calling `persia::internal::pack_path` fails the scoped run
   with E0603 and passes the workspace run). So CI's test job (and `just test`) runs `cargo test -p persia-docs`
   after `cargo test --workspace`, which still runs the doctests too. A `#[doc(hidden)]` item compiles in both, so
   the repo check of 3 also refuses names in the samples.
3. **Repo checks** (`xtask/src/repo_checks/guides.rs`, `includes.rs`). `persia_includes_nothing_outside_its_directory`,
   now `published_crates_include_nothing_outside_their_directory` (item 6): no `include_str!`, `include_bytes!` or
   `include!` (any of its three delimiters) and no `#[path = "..."]` module attribute in a `.rs` file under a
   published crate's directory (item 6) names a path outside that directory, resolved against the naming file's
   directory and normalized lexically; an argument that is not a plain string literal is refused, as it cannot be resolved.
   `guide_samples_are_doctests_of_the_public_api`: the module is wired, every page it lists exists and has a Rust
   sample, and no sample names `persia_engine`, `persia::internal`, `QueryLimits` or `Filter::from_json`.
4. **Which blocks are samples: the ones rustdoc runs** (ROADMAP 10.1.43). The check reads a fence as rustdoc does
   (`doctest` in `xtask/src/repo_checks/guides.rs`, a port of rustdoc 1.99's info-string tokenizer and of
   `LangString::parse`'s Rust rule on stable): Rust when the info string has no word, or has `rust`, or a rustdoc
   attribute (`no_run`, `ignore`, `ignore-*`, `should_panic`, `compile_fail`, `test_harness`, `standalone_crate`)
   before any other word; an `edition*` word sets the edition and makes nothing Rust (`edition2021,json` is not a
   doctest); `{.class}` and `{key=value}` attributes are no words; never when the tokenizer
   fails. A table of 67 info strings with what this toolchain's rustdoc did with each (`rustdoc --test --test-args
   --list`) is its unit test. One finding: an Expressive Code title outside braces (`rust title="src/main.rs"`)
   stops rustdoc's tokenizer at the `=`, and rustdoc then runs no doctest of the block (it only warns), so a page
   cannot use that form for a sample. Pages are found only from `#[doc = include_str!("...")]` lines outside
   comments.
5. **No guide names an item the snapshot lacks** (ROADMAP 10.1.41). `guides_name_only_the_public_api`
   (`xtask/src/repo_checks/guide_api.rs`) reads every page under `website/src/content/docs/guide/`, and by every rule but the sample one the site's
   other pages (`website/src/content/docs/*.md(x)`), against
   `crates/persia/public-api.txt`: a `persia::` path (`use` groups expanded) must be the path of a `pub` line of the
   snapshot, or a standard trait's method (`default`, `from_str`, `clone`, `from`, `to_string`) of a type the snapshot
   lists that `impl` for; a path whose first segment is a type `persia` exports (`Collection::search`,
   `Filter::from_json`), in prose or code, is read as `persia::` plus that path; no lower crate (`persia_engine`,
   `persia_blob`, `persia_format`, `persia_storage`, `persia_analysis`) is named, and no word of code (an inline span
   or a fenced line) is one of the never-public engine types of item 3 or those guides had called (`CollectionViews`,
   `QueryLimits`, `RelaxPlan`, `QueryCompiler`, `SimFile`, ...). A block the site shows as Rust must be on a page
   `guides.rs` runs, and on such a page every block rustdoc reads as Rust must run. Pages that describe the engine on
   purpose (the architecture, the file format, ranking internals) mark each such part between
   `<!-- engine-internals -->` and `<!-- /engine-internals -->` (MDX: `{/* engine-internals */}` and
   `{/* /engine-internals */}`), whose first line says in prose that it names engine internals. Inside those parts only the
   engine-crate and engine-type word rules and the paths by type name are skipped: a `persia::` path is still
   checked against the snapshot, except `persia::internal` and the paths under it (the hidden layer, named there as
   an internal); an unclosed, nested or unannounced part fails. Paths may start with `::` and have blanks around
   `::`, and inline code is every backtick span and `<code>` element. The status sentences
   `website/src/status.mjs` puts into pages (`%%TOKEN%%`) are checked by `website/src/status.test.mjs`, which reads
   the same snapshot for the calls it names (run by `website.yml`'s build job, ROADMAP 10.1.52, which also runs on a
   change to the snapshot or to any other file outside `website/` that `status.mjs`, `progress.mjs` or the node
   tests read: the engine sources, `crates/persia/src/lib.rs` and `internal/blob.rs`, `persia-blob`'s `lib.rs`,
   `persia-analysis`'s `registry.rs`, `Cargo.toml`, `ROADMAP.md` and `PROGRESS.md`; `website/src/paths-filter.test.mjs`
   fails when a path they read is outside the `pull_request` paths filter or the `push` list differs from it). A
   method named on a value (`page.next_cursor()`) and a renamed import
   (`use persia as p; p::internal`) are not resolved: the check does not type the text, and rustdoc compiling the
   samples (run alone, `cargo test -p persia-docs`) refuses those that name nothing public.
6. **The include check covers every published crate** (ROADMAP 10.1.53). `persia-engine`'s integration tests
   included 11 committed fuzz seeds from `fuzz/corpus/`, which the engine's package would not ship. They now read
   copies under `crates/persia-engine/tests/fixtures/fuzz_seeds/<target>/<name>` (a few hundred bytes in all;
   `binary` by `.gitattributes`' `**/tests/fixtures/**`), and `fuzz_seed_copies_equal_the_corpus`
   (`xtask/src/repo_checks/includes.rs`) requires each copy, in any member's `tests/fixtures/fuzz_seeds/`, to be
   byte for byte its `fuzz/corpus/` file: the corpus stays the source (cargo-fuzz reads it, and the tests check the
   seeds it runs), so a seed changed or removed there fails until its copy follows. `persia-format`'s two seed
   tests (`frame_props.rs`, `container_header_props.rs`), which read their 14 seeds at run time rather than
   including them, read copies under `crates/persia-format/tests/fixtures/fuzz_seeds/` the same way (about 80 KB,
   the container headers' full slots). Copies rather than moving the
   tests into a never-published crate as item 1 did: they test engine internals (`Document::decode`,
   `Cursor::from_token`, the query compiler's property test), which a crate depending on `persia` cannot reach, and a
   second never-published crate depending on `persia-engine` would be a new edge for three tests. The include check,
   `published_crates_include_nothing_outside_their_directory`, runs on every crate meant to be published, read from
   the manifests: each member of the root workspace (`members`, `dir/*` expanded) unless its own manifest writes
   `publish = false` (or `publish = []`). A crate inheriting `publish.workspace = true` is held back only by the
   workspace's `false` of today and is checked; `persia-docs`, `persia-testutil` (a dev-dependency only) and
   `xtask` (repo tooling) write `false` themselves; `fuzz/` is its own workspace. A new crate is checked unless its
   manifest opts out in the same way. What this leaves open, ROADMAP 10.1.55: the check reads compile-time
   includes only, and two gaps of the same kind remain. `persia-testutil` is a path-only dev-dependency of
   `persia-engine`, which `cargo package` strips, so the engine tests that use it (`relax_zero_results.rs`,
   `relax_quality.rs`) do not build from the engine's package; and tests of published crates read files outside
   their directory at run time (the golden files under `tests/fixtures/` of the root, among others).
7. **A sample shown with a fixed path equals a doctest** (ROADMAP 10.3.3). The README's quickstart opens
   `app.persia` in the working directory, so it pastes into a fresh `cargo new` with `persia` as its one
   dependency. It is not a page of `crates/persia-docs`: as a doctest it would write `app.persia` into that crate
   and fail its second run (`create_collection_json` refuses an existing collection), and a doctest cannot move to
   a temporary working directory (merged doctests share one process, so `set_current_dir` would race the others).
   A temporary directory in the README itself would make the user add `tempfile` and lose the file the sample
   says it creates. So `shown_samples_equal_a_doctest` (`xtask/src/repo_checks/samples.rs`) requires the shown
   block to equal a twin, a Rust block rustdoc runs on a page `crates/persia-docs` lists, once the twin is read as
   rustdoc shows it (`##` shows `#`, a hidden `# ` line is dropped), its one line binding `dir` from
   `tempfile::tempdir()` (hidden or not) is dropped, and its one `dir.path().join("app.persia")` is written
   `"app.persia"`. Nothing else may differ: a hidden line other than a comment or that binding is refused (it
   would be code the shown page lacks), so is a `dir` left after the rewrite, and the `toml` block the page shows
   before the sample must be there (both pages require it: without it the sample does not paste into a fresh
   crate) and be the twin page's first. The README's twin is the getting-started guide's first sample,
   which that page already calls the README quickstart in a temporary directory, so the two cannot drift apart
   (the README's `main` now returns `Result<(), Box<dyn std::error::Error>>`, as the guide's does). CI runs the
   check in the test job's `cargo test --workspace` and the twin in its `cargo test -p persia-docs`.
8. **The landing page's sample is the README quickstart, checked the same way** (ROADMAP 10.1.51).
   `website/src/content/docs/index.mdx` shows a `rust title="src/main.rs"` block, which rustdoc does not collect
   (item 4), opening `app.persia` as the README does. A title rustdoc reads (Expressive Code also takes one from a
   first-line `// src/main.rs` comment) would not be enough: run as shown, the fixed path has item 7's problem, and a
   temporary directory on the page would change the hero. So the hero shows the README's program and its `toml`
   block, line for line, and item 7's check holds it to the same twin, the getting-started guide's first sample: the
   landing page, the README and that guide are one program, which runs as a doctest. (The landing page first kept
   a shorter program of its own, with its own twin in a `#[cfg(doctest)] mod samples` of `crates/persia-docs`; the
   #141 review chose one program over two twins.) The check is in the Rust workflow (`ci.yml`, no paths filter), not
   `website.yml`, whose paths filter would skip a change to the README alone.
