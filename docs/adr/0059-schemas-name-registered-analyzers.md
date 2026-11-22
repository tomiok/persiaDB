# ADR-0059: Schemas name registered analyzers, with no format change

- Status: proposed
- Date: 2026-11-22
- Related: SPEC §6.2.2, §6.2.5, §6.3, §6.3.3, §8.6, §8.7, §15; ROADMAP 6.6.7, 6.6.8, 8.1.23, 8.1.25; ADR-0022
  (amends item 8), ADR-0024, ADR-0033

## Context

`AnalyzerRegistry::register` let a caller add an analyzer, but a schema could name only built-in ones
(`AnalyzerRef` resolved built-ins only, ADR-0022 item 8), so a registered analyzer could never index a field.
`persia verify` could rely on that: it replays the WAL with the built-in analyzers. Opening the door raises three
questions: how a schema names a registered analyzer without changing the stored bytes, what a build that lacks the
analyzer does with such a schema, and how a tool without the caller's analyzers (the CLI) still checks the database.

## Decision

1. **Same bytes.** A registered analyzer is named as a built-in is: `"name"` (its highest registered version) or
   `"name@version"` in a definition, `"name@version"` in a stored schema, and `str name, varint u32 version` under
   tag 1 of the canonical encoding (SPEC §6.2.5). No layout, encoding version or manifest version changes; a golden
   vector pins a schema naming one.
2. **Names of built-in shape.** `register` accepts only `[a-z][a-z0-9_-]*` (`persia_analysis::is_analyzer_name`) and
   still refuses built-in names at any version. Builds from before this change read a stored `name@version` of that
   shape that is not one of their built-ins as written by a newer build (`UnsupportedAnalyzer`), so they report such
   a database unsupported, never corrupt. A test checks that rule over every name `register` accepts.
3. **Definitions resolve through a registry.** `CollectionSchema::from_json_in(json, &registry)` and
   `AnalyzerRef::parse_in` accept the registry's analyzers besides the built-ins; `from_json`, `AnalyzerRef::parse` and
   the serde `Deserialize` impl stay built-in only. A built-in name resolves to a built-in version whatever the
   registry holds.
4. **Stored schemas decode without a registry.** The manifest decoder reads a well-formed `name@version` as: a
   built-in version it has; a built-in name at a version it lacks, `UnsupportedAnalyzer` (as before); any other name,
   a registered analyzer, which decodes. A malformed one stays corruption.
5. **The open checks its registry.** A database (writer or read-only) opens with a registry
   (`DatabaseOptions::registry`, `ReadOnlyDatabase::open`'s argument) to which the built-ins are always added
   (`AnalyzerRegistry::with_builtins`). A stored schema naming an analyzer the registry lacks fails the open with
   `DatabaseError::UnregisteredAnalyzer`, which is not corruption, before any segment is read or record replayed: the
   open could neither query the collection nor keep it intact on its next commit.
6. **Verification skips analysis, visibly.** `ReadOnlyDatabase::open_skipping_analysis` (and `_container_` form)
   registers, for each such field, an analyzer that emits no token under its name and version, so replay still
   decodes, validates and applies every record, and lists the field (`skipped_analysis`). `persia verify` opens that
   way with the built-in analyzers, runs every other check, and prints an `analysis` row naming the skipped fields;
   a skip is not a failure (exit status 0, SPEC §8.6). Such a snapshot cannot answer a query on those fields: a
   query compiler built with the same registry fails on them.
7. **`persia_engine::analysis`** re-exports `persia_analysis`, whose registry the engine's public API takes, for
   crates that reach the engine only (the CLI's tests).

## Consequences

- A newer build's built-in that this build lacks under a name it does not know (`nl@1`) is indistinguishable from a
  registered analyzer: this build reports it unregistered rather than unsupported. Both fail the open, neither is
  corruption.
- A caller that registered a name a later release makes a built-in gets `RegisterError::Reserved` from that release,
  before its database opens. A caller that ignores the error would open the database with the built-in chain in
  place of its own; only a stored fingerprint of a registered chain could catch that (not done).
- The stability promise of SPEC §6.3 (a released version never changes its chain) now binds the caller for its own
  analyzers; the engine cannot check it.
- A database's integrity can be checked without its custom analyzers, but terms of those fields in the WAL are not
  checked against the 32 KiB token limit (SPEC §6.3.1) by `verify`; a writer still checks them.

## Alternatives considered

- **A new schema option or tag for "registered"** (`"registered": true`, or tag 5 in the canonical encoding): a format
  change for information the name already carries; older builds would report it unsupported anyway.
- **A reserved prefix for registered names** (`x-…`): avoids collisions with future built-ins, but forbids natural
  names and still needs the same open check; the shape rule plus `Reserved` already makes a collision loud.
- **Pass the registry to the manifest decoder:** every decoder (remote readers, inspection) would need the caller's
  analyzers just to read a manifest; the open is where analysis is needed, so the check belongs there.
- **`persia verify` loading analyzers from a plugin or a config file:** no stable ABI for analyzers, and against the
  no-config pillar; skipping analysis checks everything but the terms.
