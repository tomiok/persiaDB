# ADR-0023: Document model, canonical floats and the CBOR document encoding

- Status: proposed
- Date: 2026-10-19
- Related: SPEC §5.2.1, §6.1, §6.2.2, §6.4 (§6.4.1–§6.4.2), §9.1; ROADMAP 3.2.1–3.2.6, 5.2.6

## Context

SPEC §6.4 said only that documents arrive as JSON and are stored as CBOR in `DocStore`. Before the DocStore writer
(ROADMAP 5.2.6) persists rows, the model needs fixed rules: what value each field type takes, which fields are
multi-valued, how `datetime` looks in JSON, the limits, what happens to `-0.0` and NaN (the order-preserving keys of
§5.2.1 would otherwise index `-0.0` below `0.0` and NaN at the ends, so a range filter could miss them; ROADMAP 3.2.6),
and exactly which bytes encode a document. DocStore rows are persisted, so the encoding falls under the format
discipline and must have one encoding per document.

## Decision

1. **Values by type** (SPEC §6.4.1). `text`/`keyword`: a string or a list of strings. `i64`, `f64`, `bool`,
   `datetime`: one value. Doc values (Columns, §5.2.1) hold one value per document, so multi-valued numerics would
   need another column layout; strings are where multiple values are common (tags), and keyword columns are defined
   with their writer (M5). `stored`: any JSON value. `null` means absent. An empty list is kept.
2. **Floats.** NaN and infinities are rejected; `-0.0` becomes `0.0` when it enters a document (`Document::insert`,
   JSON), so no writer ever keys them and a decoder treats them in an `f64` field as corruption. Rejecting `-0.0`
   instead would turn ordinary computed values into write errors. Numbers inside `stored` values are opaque and kept.
3. **`datetime`** is microseconds since the epoch in years 0000–9999: JSON takes an integer or an RFC 3339 string
   with an offset and at most 6 fractional digits (more would be silently truncated); output is RFC 3339 UTC. The
   range keeps the output a valid RFC 3339 timestamp.
4. **Non-strict collections** keep unknown keys as extra `stored` values by name (they have no field id). Their
   names follow the field name rules, so a later schema can add a field of that name. An old row's extra field
   stays an extra field after that; an extra field next to the schema field of the same name is corruption.
5. **Limits**: id 1–512 bytes (§6.1), keyword value 32 KiB (below what term dictionaries handle comfortably),
   1024 fields per document (schema and extra fields together), 64 levels of nesting in stored values (bounds decoder recursion),
   1 048 576 values (field values, list elements, values inside stored values: caps what decoding a hostile row of
   one-byte items allocates at about 32 MiB), 16 MiB of JSON and 16 MiB of encoding. JSON parsing rejects duplicate
   keys at every level.
6. **Encoding**: a hand-written, deterministic subset of CBOR (RFC 8949): definite lengths, shortest arguments,
   no tags, floats always 64-bit, map keys strictly increasing; a document is `array(5)[version, id, blob, fields,
   extra]`, fields keyed by field id, the blob reference as its ADR-0011 bytes. The version is read before the
   array length is checked, so a later version may change the arity. Decoding is schema-driven: the field
   type decides how an integer is read (`i64` vs `datetime`). The decoder enforces every structural and value rule,
   so anything it accepts re-encodes to the same bytes, except that a row may hold what a new document could not
   (decision 4, or extras of a collection that became `strict`). Such rows are returned as they are; merges copy rows
   as bytes rather than re-validating them. Golden vectors pin the bytes.
7. **DocStore rows** hold the stored fields only (`stored: true`), plus id, blob reference and extra fields.
   The same encoding with every field is available for other persisted uses (WAL records, M6).

## Consequences

- Range filters and sorts on `f64` see one zero and no NaN.
- A list of numbers or dates is not expressible in a numeric field; users store it in a `stored` field or as
  keywords.
- The codec is in `persia-engine` (`document::cbor`) rather than a crate: ~200 lines, with strictness no general
  CBOR crate offers (shortest-form and key-order checks), and fuzzed (`document`, `document_json`).
- A future encoding change is a new `version` gated like the blob reference's (SPEC §9.1, §15).

## Alternatives considered

- **`ciborium`** (allowed): accepts non-canonical input and needs a schema-free value tree; checking canonical form
  on top would re-implement most of the decoder.
- **Store the JSON text as given**: preserves formatting, but larger, schema-unaware, and two encodings per document.
- **RFC 8949 §4.2.1 key order** (bytewise over encoded keys, so shorter keys first): equivalent in strength; plain
  byte order of the UTF-8 key matches `BTreeMap<String, _>` and the blob reference's `user_meta` rule.
- **Multi-valued numerics now**: would fix the column layout before M5 designs it.
