# ADR-0062: The request JSON, the engine's typed request, and the cursor token

- Status: proposed
- Date: 2026-11-23
- Related: SPEC §7.1, §7.1.4, §7.2, §7.4, §7.6, §17; ROADMAP 10.1.3, 10.1.7, 10.1.10, 10.1.13, 10.1.14, 10.1.20,
  10.1.21, 8.2.13; ADR-0022, ADR-0036, ADR-0045, ADR-0048, ADR-0056

## Context

SPEC §7.1 shows a request as JSON, but until M10 the engine had only its parts: `Filter::from_json`,
`RelaxOptions::from_json`, `DefaultFields::parse`, `QueryCompiler::compile`, `Compiled::with_filter` /
`with_soft_filter`, the typed `db::SearchRequest` (the page, without a query), and the two searches (`search_with`,
`search_relaxed`). Every caller that wanted "run this request" had to compose them, and each composition is a chance
to get one rule wrong. The rule that matters most is ROADMAP 8.2.13's: a request's `filter` is hard only when the
relaxed search is given the compilation (`RelaxedQuery::from(&Compiled)`); given the bare query (`From<&Query>`) it is
a programmatic query whose root filters a caller may mark soft.

Three callers need the composition: the embedded API's typed `search` and its `search_json` (ROADMAP 10.1.3), and
later the server (M11). The embedded bindings (ADR-0018) pass requests across the C ABI as JSON, so the JSON is not
only a server concern. A cursor also has to cross JSON, and `Cursor` is a struct of engine values (sort values, a
segment index, a docid, relaxation settings) that no client should build or read.

## Decision

1. **The request JSON is parsed in the engine**, in `persia_engine::search::request`, into one typed value,
   `QueryRequest`. The engine owns every part's grammar already (filters, `relax`, `fields`, the schema the names
   resolve against), so a parser anywhere else would either re-implement them or reach into the engine for each. The
   facade (`persia`) keeps the public `SearchRequest` builder and lowers both it and `search_json` to `QueryRequest`;
   the server will do the same. The engine's `db::SearchRequest` keeps its name and stays the page part of a request.
2. **One function runs it**: `QueryRequest::run(&CollectionSnapshot, &RunOptions)`. It compiles `q` with the
   collection's own compiler, joins `filter` (hard) and `soft_filter` (soft), validates the joined query against the
   limits, merges the relaxation settings (collection, then request), and runs the relaxed search with the
   compilation when the settings relax, the plain search when they run L0 alone. No caller composes the parts again,
   so none can pass the bare query. `RunOptions` carries what is not part of the request: threads, a deadline, a
   cancellation token, the budget clock and the query limits. A caller names neither `SearchPlan`, `Execution`, the
   query AST nor the compiler.
3. **A collection has one query compiler**, held by its snapshots (`CollectionSnapshot::compiler`) and built from the
   analyzers its memtable indexes with, so a query is analyzed exactly as the documents were, including registered
   analyzers (ADR-0059) and the fields a read-only open skips. It is shared by the collection's snapshots and replaced
   only when the schema evolves: the synonym maps it analyzes on first use, and its cache of alternatives (ADR-0058),
   are built once per schema, not once per request.
4. **Strict reading**, as a collection definition (ADR-0022) and a filter are read: an unknown key, a key repeated at
   any depth, `null` for a key and a value of another shape are errors. A key left out has its default. `q` may be
   left out (empty text, which matches every document): a filter-only request is the browse case §7.1.1 already
   defines. Integers (`limit`, `offset`, a facet `size`) are JSON integers, not `20.0` or `2e1`.
5. **One input bound: 1 MiB for the whole request**, the bound a filter's JSON already has. A request holds its
   filters, so a larger bound would be needed only to hold two near-maximal filters, and the parts cannot be measured
   separately once the text is parsed. Inside a request, `relax`'s own 64 KiB bound does not apply: its strict reader
   refuses unknown keys, and what it accepts is bounded by the request.
6. **Limits are enforced twice, at the two places they can be exceeded.** `from_json` takes the limits and holds each
   filter to them (as `Filter::from_json` does), so a server refuses an oversized filter before it runs anything.
   `run` holds the compiled `q` joined to both filters to `RunOptions`' limits, because the join adds nodes, terms
   and depth (SPEC §7.1.3), and gives the relaxed search the same limits for its steps.
7. **`facets` entries are a field name or `{"field": name, "size": n}`.** SPEC §7.5 gave each facet a `size` but
   only the name form in JSON; the object form uses the two words the SPEC has, in the shape `sort`'s field key has.
8. **No JSON key for the projection yet** (SPEC §7.5.1 defines the projection, not a key). The typed request carries
   it (`with_projection`); the key is decided with the response JSON (ROADMAP 10.1.20).
9. **The plain search for `relax: false`.** The request path returns the engine's two result types as they are
   (`QueryResults::Relaxed` / `Plain`) plus `query_truncated`. A plain search does not count in the relaxation
   metrics and its explained hits have no `relaxed` node; a relaxed search with an L0-only plan would do both. This
   ADR records the behaviour, not a choice between them: ROADMAP 10.1.21 asks whether zero-result `relax: false`
   requests belong in `empty`.
10. **`"proximity": true` needs a search that does not relax** (ROADMAP 10.1.7). The relaxed search plans every step
    without the bonus (SPEC §7.2), so asking for it on a search that relaxes is an error that names the fix
    (`"relax": false`), not a silently ignored option. The bonus across relaxed steps is ROADMAP 10.1.14.
11. **A cursor crosses JSON as an opaque token**: base64url without padding (a hand-written codec, no new
    dependency) of a binary encoding that starts with a version byte and ends with the CRC-32C of the bytes before
    it (SPEC §7.4 has the layout). `Cursor::to_token` / `Cursor::from_token`. It is not an on-disk format: nothing
    stores it. But clients hold tokens between requests, so **the version byte is the compatibility rule**: any
    change to the layout or to the meaning of a field takes a new version, a build refuses versions it does not read,
    and a golden test per version pins the text. Decoding is strict (one encoding per cursor) and never panics.
12. **The checksum is not authentication.** It turns a token that was cut or mistyped into an error instead of
    another cursor. A client can still build any cursor it likes, so a decoded cursor is checked exactly as a typed
    one is: against the sort, the snapshot and memtable view, the collection, the proximity setting and the
    relaxation settings (SPEC §7.4). A cursor only names a place in a ranking the caller may already read, so there is
    nothing for a signature to protect, and a key would be state the embedded database does not have.

## Consequences

- `persia` (ROADMAP 10.1.3) builds `search` and `search_json` on `QueryRequest::run`, and maps
  `QueryRequestError`: everything is the caller's error except a view that cannot be read
  (`RequestError::Read`, `SearchError::Read`) and an interrupt (`QueryRequestError::interrupt`).
- A snapshot of a collection now holds an `Arc<QueryCompiler>`. Opening or evolving a collection clones its schema
  into the compiler once.
- A request is bound to one schema (field ids). `run` refuses default fields of another schema, which `compile`
  would otherwise only `debug_assert`, and checks the sort and the facets against the snapshot's schema again (a few
  comparisons per request); the filters are checked with the joined query.
- A plain search's cursor and one of an L0-only cascade page each other (they record no settings, ADR-0045), so
  10.1.21 can change which search `relax: false` runs without breaking tokens.
- A token's length grows with keyword sort values (up to 32 KiB each); it is bounded by the request's 1 MiB.
- Fuzz targets `request_json` and `cursor_token`; the response JSON, which writes tokens, is ROADMAP 10.1.13.

## Alternatives considered

- **Parse in the facade (`persia`), or in the server.** Each would need `Filter`'s and `relax`'s parsers anyway, and
  the second caller (C ABI, server) would repeat the composition. The grammar and the composition are one decision,
  so they live where the parts are.
- **A cursor as readable JSON** (`{"values": [...], "segment": 2, "docid": 7, ...}`). It would make engine internals
  (segment indexes, relaxation step numbers, level sets) a public, documented surface that clients would then
  construct and depend on. An opaque token keeps them free to change behind the version byte.
- **Standard base64, or hex.** Standard base64 needs escaping in URLs (`+`, `/`, `=`), where the HTTP API will carry
  tokens; hex doubles the length. A `base64` crate is outside the allowed list, and the codec is 60 lines.
- **An HMAC instead of a CRC.** See decision 12: there is no secret to protect and no key to keep.
- **A relaxed search with an L0-only plan for `relax: false`,** so every response has a `relaxation` value. It cannot
  score the proximity bonus without a change to `search_relaxed`, which decision 10 leaves for ROADMAP 10.1.14.
- **`serde` derive for the request.** The parts are already parsed by hand over `serde_json::Value` with the strict
  duplicate-key reader; deriving would need a second set of name-based types to resolve against the schema afterwards.
