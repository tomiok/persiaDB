# ADR-0065: The response JSON, the request's `projection` key, and `relax: false` in the metrics

- Status: proposed
- Date: 2026-11-25
- Related: SPEC §7.1.4, §7.4, §7.5.1, §7.6, §7.7; ROADMAP 10.1.13, 10.1.20, 10.1.21; ADR-0062

## Context

ADR-0062 item 8 left the request JSON without a key for the projection of SPEC §7.5.1 (every stored field, none, or a
list of names): a request built in code set it (`QueryRequest::with_projection`), and a parsed one returned every
stored field. The key was to be decided with the response JSON (ROADMAP 10.1.20), the JSON a search response is
written as (ROADMAP 10.1.13), which the server (M11) and the embedded bindings (ADR-0018) need as much as they need the
request JSON.

## Decision

1. **The key is `projection`**, with three forms: `"all"` (every stored and extra field, also when the key is left
   out), `"none"` (the id and the blob reference only), or a non-empty array of distinct names. Each name is resolved
   as `Projection::fields` resolves it (SPEC §7.5.1): a field of the schema with `stored: true`, or, in a collection
   that is not `strict`, an extra field. It is read as strictly as every other key (SPEC §7.1.4): another string
   (`"All"`, `""`), another shape, `[]`, and a name given twice are shape errors that name the key; a name that is not
   stored, or not a field of a `strict` collection, is the request error of SPEC §7.5.1 (`RequestError::Projection`),
   both an invalid argument. `[]` is refused rather than read as `"none"`, because some callers would read it as "no
   restriction"; `"none"` says it. The message names a long name by its length, as every echoed name is (SPEC §7.1.4).
2. **The engine writes the response JSON**, `QueryResponse::to_json(&schema)` in `persia_engine::search::request`,
   beside the request JSON (ADR-0062 item 1): the response holds engine values (field ids, column keys, keyword
   bytes, explanation trees) that only the engine and the schema can name, and every caller (the facade, the C ABI,
   the server) then writes the same text. SPEC §7.5.2 is the format.
3. **Written by hand, keys in a fixed order.** `serde_json`'s map sorts its keys (the workspace does not turn on
   `preserve_order`), and `corrected` is in query order; a writer of a few helpers keeps every object in SPEC's order,
   so a golden response pins the text. Leaves (strings, numbers) are still `serde_json`'s, so escaping and the
   shortest round-trip form of an `f64` are its. The document is its own JSON form of SPEC §6.4.1
   (`Document::to_json`), embedded as `document`: one form of a document everywhere.
4. **Absent is left out, never `null`.** The one `null` is a missing sort value, which keeps its position among the
   hit's sort values. Lists and maps that may be empty (`applied`, `corrected`, `facets`) are always written.
5. **Sort values are typed by their key**, not written as the column key the cursor compares (SPEC §7.4): an `i64`
   integer, an `f64` number, a `datetime` in the document's output text, a `bool`, a keyword value.
6. **A keyword value that is not UTF-8 is `{"base64url": "..."}`**, with the cursor token's codec (ADR-0062 item 11);
   a UTF-8 one is a plain string. Documents only hold keyword strings, but facet values, sort values and explained
   terms are bytes the index holds, and a JSON string cannot hold arbitrary bytes. An object, not a string with a
   prefix, so no UTF-8 value can be mistaken for an encoded one.
7. **`corrected` is the object SPEC §7.6 shows**, from each word to the term searched for it, in query order; a word
   the query repeats is named once (its corrections are the same, and an object with a repeated key would be read
   differently by different readers).
8. **A plain search states L0's text**: `original` as read and `effective` the clauses that compiled
   (`QueryResponse::relaxation`), as a relaxed search that stops at L0 states them, with `level: "exact"`, `applied:
   []` and `truncated: false`. Its hits have no `level`, `step` or `rank_score`: the plain search has no steps.
9. **Writing fails only on a broken invariant** (`ResponseJsonError`): a field id of another schema, a value of
   another type than its field, a number that is not finite. A response the engine returned for a collection, written
   with that collection's schema, always has its JSON (the request fuzz target asserts it).
10. **A request of L0 alone counts in the relaxation metrics, on the plain search** (ROADMAP 10.1.21; amends ADR-0062
    item 9). A request whose settings run L0 alone (`relax: false`, a collection that disables the cascade) still runs
    the plain search, which scores the proximity bonus and explains without a `relaxed` node, and its page from the
    top counts as a query at level `exact`: in `queries`, never in `relaxed`, in `empty` when L0 found no hit, else in
    the `exact` count. So `empty / queries` is the zero-result rate of every request, `relax: false` included, which is
    what an operator reads it for. The plain search collects one hit at least when it counts, so a page by `offset`
    past the hits, or of `limit` 0, still tells whether L0 found one (as the relaxed search's `min_results` of at least
    1 does). A cursor's page counts nowhere, as for the relaxed search. The engine's own plain search, run without a
    request (`CollectionViews::search_with`), counts nowhere: it is not a query a user made. Switching `relax: false`
    to the relaxed search with an L0-only plan would have counted too, but would drop the proximity bonus (ROADMAP
    10.1.14) and change every explained hit's root.

## Consequences

- `QueryRequest::from_json` sets `SearchRequest::projection`; the facade's `search_json` takes the key with no change.
- The request fuzz target reaches the key through its JSON input, and asserts that every response it gets is written
  as valid JSON.
- The facade's `SearchResponse` is a converted value (it drops the step, the rank score and the sort values), so it
  does not write this JSON yet; a caller of the engine does (ROADMAP 10.1.25).
- A response is at most `limit` hits of documents of at most 16 MiB each (SPEC §6.4.2), so its text is bounded by the
  page, as the typed response is.

## Alternatives considered

- **`serde` derive over response types.** The engine's response types hold field ids and column keys, so deriving
  would write ids and keys, or need a parallel set of named types filled from them; and the derived maps would not
  keep `corrected` in query order.
- **Keyword bytes as an array of numbers, or a `\u`-escaped Latin-1 string.** Both are ambiguous with, or much longer
  than, a value a client also has to tell apart from a UTF-8 one; the object form is unambiguous.
- **`fields` for the projection,** as some search APIs name it. `fields` is already the default fields `q` searches
  (SPEC §7.1), so the key needs another name.
- **`"*"` or `true` for every field.** A word reads better than a symbol, and `"all"` / `"none"` mirror each other; a
  boolean would leave the list form as a third type for one key.
- **Ignoring a repeated name.** Nothing in a request is guessed (SPEC §7.1.4): a repeated name is likely a mistake
  in the program that wrote it.
