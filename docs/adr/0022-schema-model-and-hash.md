# ADR-0022: Collection schema model, strict JSON and the schema hash

- Status: proposed
- Date: 2026-10-19
- Related: SPEC §6.2 (§6.2.1–§6.2.5), §6.3, §7.6, §4.1; ROADMAP 3.1.1–3.1.4, 3.3.5

## Context

SPEC §6.2 showed a collection definition by example only. Before segments (M5) record a schema hash and the manifest
(M3.3) stores schemas, the model needs fixed rules: which keys exist, their defaults and ranges, how fields get the
numeric ids segments use, which changes a live collection accepts, and exactly which bytes the hash covers. The hash
is persisted in every segment, so it must not drift across releases, dependency upgrades or JSON key order.

## Decision

1. **Strict parsing.** The JSON definition is the user-facing form (embedded API, server, SDKs). Unknown keys at any
   level, duplicate keys (serde_json keeps the last one silently in a map, so `fields` uses a map type that rejects
   them) and options that do not apply to the field's type are errors. A typo such as `"postions": true` must not
   silently create a field without positions. Ranges and limits are in SPEC §6.2.1–§6.2.3.
2. **Two layers in code.** serde types mirror the JSON (`deny_unknown_fields`); `CollectionSchema` is built from them by
   validation and has private fields, so every value of the type is valid. Serializing writes every option explicitly
   (`"analyzer": "es@1"`, defaults included), so a stored schema means the same thing to a later release whose
   defaults differ.
3. **Field ids** are assigned from the names (ascending byte order) when the definition has none, so the same
   definition always gets the same ids. Evolution matches fields by name: existing fields keep their ids, added
   fields take the next ones. Fields are never removed in v1, so ids stay dense (`0..n`), and the id is the position.
4. **Evolution** accepts adding fields and changing query-time settings; it rejects anything that changes what is
   indexed (type, analyzer name or version, positions, facet, stored) or the collection name (SPEC §6.2.4).
5. **Hash = XXH3-128 over a hand-written canonical encoding** of the index definition only (SPEC §6.2.5): version
   byte, name, then per field its name, type tag and tagged options in ascending tag order. Query-time settings are
   left out, so tuning boosts or relaxation does not make old segments look foreign. The encoding is encode-only
   (nothing parses it), and golden vectors in the tests pin both bytes and hash.
6. **Extending without changing old hashes.** A new option gets a new tag and is written only when its value differs
   from the behavior before it existed; any other change is a new `encoding_version`.
7. **Analyzer registry stub.** Until M4 (ROADMAP 4.4.1, 4.4.3) provides one, the schema module lists the built-in
   analyzer names of SPEC §6.3 at version 1 and rejects anything else.

## Consequences

- A definition is either fully valid or rejected with the path of the first problem; nothing is ignored.
- Adding a field changes the hash. Segments written before carry the earlier hash, so the manifest has to keep the
  hashes of earlier schema versions still referenced by live segments (new leaf ROADMAP 3.3.5).
- The query-time settings live in the same schema but outside the hash; the manifest (3.3) persists the whole schema.
- The built-in analyzer list duplicates SPEC §6.3 until M4 replaces it with the real registry.

## Alternatives considered

- **Hash the serde/CBOR (or JSON) form.** Its bytes depend on library behavior (float width, map order) and on every
  setting, including query-time ones; rejected for a persisted identifier.
- **Hash the whole schema.** Tuning `boost` or `min_results` would make every existing segment fail verification.
- **SHA-256.** Nothing adversarial chooses schemas to collide; XXH3 is already the content hash of SPEC §4.1.
- **Field ids from declaration order.** JSON key order is not preserved by serde_json maps and is not meaningful to users.
- **Lenient parsing** (ignore unknown keys, as many JSON APIs do): typos would silently change indexing.
