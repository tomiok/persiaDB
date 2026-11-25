# ADR-0065: The response JSON and the request's `projection` key

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

## Consequences

- `QueryRequest::from_json` sets `SearchRequest::projection`; the facade's `search_json` takes the key with no change.
- The request fuzz target reaches the key through its JSON input.

## Alternatives considered

- **`fields` for the projection,** as some search APIs name it. `fields` is already the default fields `q` searches
  (SPEC §7.1), so the key needs another name.
- **`"*"` or `true` for every field.** A word reads better than a symbol, and `"all"` / `"none"` mirror each other; a
  boolean would leave the list form as a third type for one key.
- **Ignoring a repeated name.** Nothing in a request is guessed (SPEC §7.1.4): a repeated name is likely a mistake
  in the program that wrote it.
