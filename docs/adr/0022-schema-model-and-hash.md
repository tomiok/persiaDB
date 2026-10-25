# ADR-0022: Collection schema model, strict JSON and the schema hash

- Status: proposed
- Date: 2026-10-19
- Related: SPEC §5.1, §6.2 (§6.2.1–§6.2.5), §6.3, §7.6, §4.1; ROADMAP 3.1.1–3.1.4, 4.4.3, 5.2.7, 5.3.1

## Context

SPEC §6.2 showed a collection definition by example only. Before segments (M5) record a schema hash and the manifest
(M3.3) stores schemas, the model needs fixed rules: which keys exist, their defaults and ranges, how fields get the
numeric ids segments use, which changes a live collection accepts, and exactly which bytes the hash covers. The hash
is persisted in every segment, so it must not drift across releases, dependency upgrades or JSON key order.

## Decision

1. **Strict parsing.** The JSON definition is the user-facing form (embedded API, server, SDKs). Unknown keys at any
   level, duplicate keys (serde_json keeps the last one silently in a map, so `fields` uses a map type that rejects
   them) and options that do not apply to the field's type are errors. A typo such as `"postions": true` must not
   silently create a field without positions. Every level must be a JSON object: derived serde visitors also accept
   an array read by position, which would bypass key names, so an object-only adapter wraps each struct. Ranges and
   limits are in SPEC §6.2.1–§6.2.3; the whole definition is at most 4 MiB and synonyms at most 1 MiB, which bounds
   what parsing allocates. Both checks need the text: `from_json` is the entry point for untrusted input, and the serde
   `Deserialize` impl (no size bound; a `serde_json::Value` has already merged duplicate keys) is for trusted,
   already-bounded sources such as the manifest.
2. **Two layers in code.** serde types mirror the JSON (`deny_unknown_fields`); `CollectionSchema` is built from them by
   validation and has private fields, so every value of the type is valid. Serializing writes every option explicitly
   (`"analyzer": "es@1"`, defaults included), so a stored schema means the same thing to a later release whose
   defaults differ. serde_json's `float_roundtrip` feature is on: its default parser can be one ULP off, so a
   schema read back would differ from the one written (the hash covers no floats; equality does).
3. **Field ids** are assigned from the names (ascending byte order) when the definition has none, so the same
   definition always gets the same ids. Evolution matches fields by name: existing fields keep their ids, added
   fields take the next ones in ascending byte order of their names. Fields are never removed in v1, so ids stay dense (`0..n`), and the id is the position.
4. **Evolution** accepts adding fields and changing query-time settings; it rejects anything that changes what is
   indexed (type, analyzer name or version, positions, facet, stored) or the collection name (SPEC §6.2.4).
5. **Hash = XXH3-128 over a hand-written canonical encoding** of the index definition only (SPEC §6.2.5): version
   byte, name, the `field_count` varint, then per field in ascending field id its name, type tag and tagged options
   in ascending tag order. Query-time settings are
   left out, so tuning boosts or relaxation does not make old segments look foreign. The encoding is encode-only
   (nothing parses it), and golden vectors in the tests pin both bytes and hash.
6. **Extending without changing old hashes.** A new option gets a new tag and is written only when its value differs
   from the behavior before it existed; any other change is a new `encoding_version`. Readers keep the encoder of
   every version they accept, and a segment records the `encoding_version` with the hash.
7. **Old segments verify by truncation.** A segment also records how many fields it was indexed with. Evolution
   only appends fields and never changes what the hash covers, so an earlier version's hash is the current
   schema's hash over its first `field_count` fields (`CollectionSchema::hash_at`); no history of hashes is kept.
8. **Analyzer registry.** The schema accepts exactly the built-in analyzer versions `persia-analysis` ships
   (`BUILTIN_ANALYZERS`, SPEC §6.3.3) and rejects anything else. *Amended in ROADMAP 4.4.3:* this replaced a stub
   list in the schema module that also named `fr`, `de` and `it`, which have no analyzer yet (ROADMAP 4.4.10).

## Consequences

- A definition is either fully valid or rejected with its first problem (a field or option name, or serde_json's
  line and column); nothing is ignored, except ids in a definition proposed for evolution, which are internal.
- Adding a field changes the hash; segments written before still verify through `hash_at` (decision 7).
- An unversioned `analyzer` resolves to the current version in a new schema, and binds to the field's version on
  evolution (SPEC §6.2.4). `AnalyzerRef` remembers whether the definition named the version; that flag is not part
  of its identity or of the hash.
- The query-time settings live in the same schema but outside the hash; the manifest (3.3) persists the whole schema.
  If it persists the serde form, that form becomes an on-disk format and falls under the format discipline then.

## Alternatives considered

- **Hash the serde/CBOR (or JSON) form.** Its bytes depend on library behavior (float width, map order) and on every
  setting, including query-time ones; rejected for a persisted identifier.
- **Hash the whole schema.** Tuning `boost` or `min_results` would make every existing segment fail verification.
- **SHA-256.** Nothing adversarial chooses schemas to collide; XXH3 is already the content hash of SPEC §4.1.
- **Keep a history of hashes in the manifest** for segments indexed before fields were added. Truncation (decision 7)
  needs no extra state, because evolution only appends.
- **Field ids from declaration order.** JSON key order is not preserved by serde_json maps and is not meaningful to users.
- **Lenient parsing** (ignore unknown keys, as many JSON APIs do): typos would silently change indexing.
