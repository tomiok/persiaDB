# Changelog

All notable, user-visible changes to Persia DB are documented here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). On-disk format versions are tracked separately
(SPEC §15) and are called out under **Format**, a project-specific section added to the standard ones.

## [Unreleased]

### Added
- Workspace skeleton (stub crates).
- Apache-2.0 license.

### Format
- Integer codecs for segment sections (SPEC §4.1, §5.2.1; ADR-0004): zigzag signed varints, delta coding of
  sorted docids, 128-value bit-packed blocks (SIMD-BP128 layout), postings full and tail blocks, and
  frame-of-reference blocks with order-preserving keys for numeric columns.

<!--
Sections per release, in this order, only when non-empty:
### Added / ### Changed / ### Deprecated / ### Removed / ### Fixed / ### Security / ### Format
-->

[Unreleased]: https://github.com/tomiok/persiaDB/commits/main
