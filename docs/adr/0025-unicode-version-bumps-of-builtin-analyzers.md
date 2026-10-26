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
   (text, span, position) for every Unicode scalar value in a set of probe contexts, one probe per line so probes
   never join (`tests/unicode_fingerprints.rs`). The contexts are chosen to tell the UAX #29 word-break classes apart:
   alone, between letters and digits, after `a.` and `1.` (`MidLetter` vs `MidNum`: `a.x` is one word, `a.5` two),
   before a quote (`Hebrew_Letter`), doubled and around a period (self-joining classes such as `Katakana`), and next
   to an accent (case, folding, composition). A change that moves a fingerprint fails the test, which lists the
   analyzers it moved with their new fingerprints; a change visible only outside these contexts can go unnoticed, so
   the contexts grow when a review finds such a case. The test takes seconds in release and runs on every pull
   request (`ci.yml`) and nightly, so a `Cargo.lock` or toolchain change that moves a fingerprint fails its own PR.
2. **New versions for changed analyzers only.** The trigger is any moved fingerprint, not only a change of
   `UNICODE_VERSION`: a `unicode-segmentation` or `unicode-normalization` patch release can fix a table or the
   algorithm within one Unicode version. Each listed analyzer gets a new version with the same chain and records its
   new fingerprint; analyzers whose fingerprint is unchanged keep their version. Unversioned names in new definitions
   resolve to the new version (§6.2.2), and evolution keeps existing fields at theirs (§6.2.4).
3. **Superseded versions keep working.** A superseded version stays in `BUILTIN_ANALYZERS` and is built with the
   build's tables: a field indexed with it opens and is queried as before, and only text affected by the changed tables or
   algorithms (characters whose properties changed, or sequences an algorithm fix treats differently) can analyze
   differently from when it was indexed. Reindexing into a collection at the
   new version (§6.2.4) restores exact agreement. SPEC §6.3 now says what is frozen: a version's chain and tables,
   and, for the current version of each analyzer, its output under the build's Unicode version.

## Consequences

- Most bumps change few analyzers or none: Unicode keeps normalization stable for assigned characters, and many
  releases only assign new characters, which change output only for text that uses them.
- The drift of a superseded version is bounded and detectable: the build knows which versions are superseded, so a
  later layer can warn about fields still on one or offer a reindex.
- The fingerprints also catch a chain or table change that skipped its version bump, in its own PR.
- The compatibility is one-way: a newer build runs a superseded `es@1` on its tables, but an older build refuses a
  collection at `es@2` (unsupported, §6.2.2) even when its chain is the same, since it cannot know that the only
  difference is Unicode data it lacks.

## Alternatives considered

- **Reject superseded versions** (report them as unsupported, ROADMAP 4.4.11): exact, but a library update would make
  every collection on a changed analyzer unqueryable until reindexed, which breaks the zero-friction pillar for a
  change that usually touches a handful of rarely used characters.
- **Ship old Unicode tables** for superseded versions: exact and compatible, but means vendoring and maintaining
  several copies of UAX #29, normalization and case tables, which the dependency policy avoids (CLAUDE.md).
- **Fingerprint only assigned characters**: would need a General_Category table the crates do not expose; probing
  every scalar value is cheap enough and also covers newly assigned characters.
