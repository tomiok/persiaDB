# ADR-0025: Unicode version bumps of built-in analyzers

- Status: proposed
- Date: 2026-10-26
- Related: SPEC §6.3, §6.3.1, §6.3.3, §6.2.4; ROADMAP 4.1 (review), 4.4.6

## Context

Every built-in analyzer depends on Unicode data: UAX #29 word boundaries (`unicode-segmentation`), NFKC and canonical
decompositions (`unicode-normalization`), and `char` properties and case mappings (the standard library). SPEC §6.3.1
pins one Unicode version for all of them (`persia_analysis::UNICODE_VERSION`, 17.0.0), and a test fails when a
dependency or toolchain update moves any of them. SPEC §6.3 also says a released analyzer version never changes its
output, and §6.3.1 that moving the Unicode version changes the version of every analyzer whose output it changes. Two
things were missing: a way to know *which* analyzers a bump changes, and what becomes of the old versions, since a
build carries one set of Unicode tables and cannot reproduce the output of the previous ones.

## Decision

1. **Fingerprints.** Every current built-in analyzer version has a recorded fingerprint: FNV-1a 64 over its tokens
   (text, span, position) for every Unicode scalar value, alone and in a few contexts (`a·b`, `1·2`, after `é`), one
   probe per line so probes never join (`tests/unicode_fingerprints.rs`). A bump fails that test for exactly the
   analyzers whose output changed, with their new fingerprints. It takes seconds in release, so it runs in the
   nightly `--features slow --release` job and must be run for a bump; the cheap pin test stays in the default
   suite and names the command.
2. **New versions for changed analyzers only.** Each listed analyzer gets a new version with the same chain and
   records its new fingerprint; analyzers whose fingerprint is unchanged keep their version. Unversioned names in
   new definitions resolve to the new version (§6.2.2), and evolution keeps existing fields at theirs (§6.2.4).
3. **Superseded versions keep working.** A superseded version stays in `BUILTIN_ANALYZERS` and is built with the
   build's tables: a field indexed with it opens and is queried as before, and only text containing the characters
   whose properties changed can analyze differently from when it was indexed. Reindexing into a collection at the
   new version (§6.2.4) restores exact agreement. SPEC §6.3 now says what is frozen: a version's chain and tables,
   and, for the current version of each analyzer, its output under the build's Unicode version.

## Consequences

- Most bumps change few analyzers or none: Unicode keeps normalization stable for assigned characters, and many
  releases only assign new characters, which change output only for text that uses them.
- The drift of a superseded version is bounded and detectable: the build knows which versions are superseded, so a
  later layer can warn about fields still on one or offer a reindex.
- The fingerprints also catch a chain or table change that skipped its version bump, one night later.

## Alternatives considered

- **Reject superseded versions** (report them as unsupported, ROADMAP 4.4.11): exact, but a library update would make
  every collection on a changed analyzer unqueryable until reindexed, which breaks the zero-friction pillar for a
  change that usually touches a handful of rarely used characters.
- **Ship old Unicode tables** for superseded versions: exact and compatible, but means vendoring and maintaining
  several copies of UAX #29, normalization and case tables, which the dependency policy avoids (CLAUDE.md).
- **Fingerprint only assigned characters**: would need a General_Category table the crates do not expose; probing
  every scalar value is cheap enough and also covers newly assigned characters.
