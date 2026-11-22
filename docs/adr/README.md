# Architecture decision records

An ADR records one significant decision: the context, what we chose, and what it costs.
SPEC.md says *what* the system does; ADRs say *why* it is that way and what was rejected.

## When to write one

- Any change to the on-disk format (required by CLAUDE.md "Format discipline", together with a SPEC update,
  a version or flag bump and a golden test).
- Resolving an open question in SPEC §18.
- Adding a dependency outside the allowed list, or new `unsafe` (CLAUDE.md "Safety").
- Any decision a future contributor would otherwise re-litigate.

## Process

1. Copy [`0000-template.md`](./0000-template.md) to `NNNN-short-title.md`, using the next free number.
2. Open it with `Status: proposed` in the same PR as the change it justifies (or alone, for a pure decision).
3. On merge it becomes `accepted`. ADRs are never edited afterwards except for status: to change a decision,
   write a new ADR and mark the old one `superseded by ADR-XXXX`.
4. Add it to the index below and link it from the SPEC section it affects.

## Index

| ADR | Title | Status |
|---|---|---|
| [0001](./0001-own-engine-and-allowed-dependencies.md) | Own search engine, and a closed list of allowed dependencies | accepted (amended by 0003) |
| [0002](./0002-blob-hash-sha256.md) | Blobs are identified by SHA-256 | accepted |
| [0003](./0003-rust-tooling-and-no-build-scripts.md) | Repo tooling in Rust (`xtask`), and no build scripts | accepted |
| [0004](./0004-integer-block-codecs.md) | Integer block codecs (delta, bit-packing, postings blocks, FOR) | accepted |
| [0005](./0005-block-compression-codecs.md) | Block compression codecs and the decompression-bomb guard | accepted |
| [0006](./0006-block-framing.md) | Block framing of segment sections | accepted |
| [0007](./0007-container-header-and-superblock.md) | Container header and superblock validation | accepted (amended by 0008) |
| [0008](./0008-superblock-geometry-and-page-size.md) | Superblock slots on separate pages, advisory page size, monotonic format minor | accepted |
| [0009](./0009-frame-validation-and-scan-end.md) | Frame validation, scan end classes and compressed frame payloads | accepted |
| [0010](./0010-mmap-unsafe-boundary-and-sigbus.md) | The memory-mapping unsafe boundary and the SIGBUS strategy | accepted (amended by 0016, 0017) |
| [0011](./0011-blob-ref-encoding.md) | `BlobRef` value limits and binary encoding | accepted |
| [0012](./0012-local-backend-layout.md) | Local backend layout: encoded file names, directory marker, link-based `put_if_absent` | accepted (amended by 0016, 0019) |
| [0013](./0013-frame-seq.md) | `frame_seq` numbers a container's frames contiguously; the superblock anchors it | accepted |
| [0014](./0014-corrupt-tail-quarantine.md) | A read-write open quarantines a corrupt tail unless it may hold acknowledged writes | accepted |
| [0015](./0015-header-never-rewritten-in-place.md) | The header page is never rewritten in place; `format_minor` is recorded by rewrites | accepted |
| [0016](./0016-windows-behavior-and-platform-durability.md) | Windows behavior and platform durability | accepted (amended by 0021) |
| [0017](./0017-mapped-container-reads.md) | Mapped container reads: an opt-in window over the committed frames | proposed (amended by 0021) |
| [0018](./0018-embedded-sdk-bindings.md) | Embedded Go and TypeScript bindings share one C ABI | proposed (amended 2026-11-13: optional `persia_purego` tag; amended 2026-11-16: in-process core is v1.0, ROADMAP 14.6) |
| [0019](./0019-sweeping-temporary-files.md) | Stale temporary files are swept under their lock, on request only | proposed |
| [0020](./0020-cli-depends-on-storage.md) | The `persia` CLI may depend on `persia-storage` directly | proposed |
| [0021](./0021-os-calls-for-file-system-type-and-identity.md) | `libc` and `windows-sys` for the file system type and the Windows file identity | proposed |
| [0022](./0022-schema-model-and-hash.md) | Collection schema model, strict JSON and the schema hash | proposed |
| [0023](./0023-document-model-and-encoding.md) | Document model, canonical floats and the CBOR document encoding | proposed |
| [0024](./0024-manifest-encoding-and-commit-diffs.md) | Manifest encoding and commits as diffs | proposed |
| [0025](./0025-unicode-version-bumps-of-builtin-analyzers.md) | Unicode version bumps give changed built-in analyzers new versions; superseded ones keep working | proposed |
| [0026](./0026-counting-allocator-in-tests.md) | A counting global allocator in allocation tests | proposed |
| [0027](./0027-segment-footer-and-sections.md) | Segment footer, section kinds and section encodings | proposed |
| [0028](./0028-wal-chunks-and-replay.md) | WAL chunks, durability levels and replay | proposed |
| [0029](./0029-local-flush-commit-and-snapshots.md) | Local flush, commit and snapshot publication | proposed |
| [0030](./0030-tombstone-sidecars.md) | Tombstone sidecars, and upserts and deletes through the commit | proposed |
| [0031](./0031-tiered-merge-policy-and-merger.md) | Tiered merge policy, and a merger that fills a segment builder | proposed |
| [0032](./0032-background-merges-and-obsolete-objects.md) | Background merges, and obsolete objects freed by reference counting | proposed |
| [0033](./0033-open-sequence-read-only-opens-and-flushes-during-replay.md) | Open sequence, read-only opens, and flushes during replay | proposed |
| [0034](./0034-blob-upload-staging-and-expected-id.md) | Blob upload staging, binding, and the expected-id API | accepted (amended by 0051) |
| [0035](./0035-blob-manifest-encoding.md) | Blob manifest encoding | accepted (amended by 0041, 0050) |
| [0036](./0036-term-proximity-scoring.md) | Term proximity scoring: pair distance of adjacent required words, bound-safe, off by default | accepted |
| [0037](./0037-relaxation-cascade-and-level-rank-key.md) | The relaxation cascade, and a rank key led by the level | proposed |
| [0038](./0038-l2-drop-terms-ladder-and-term-importance.md) | L2 `drop_terms`: a `minimum_should_match` ladder ordered by term importance | proposed |
| [0039](./0039-l1-synonyms-runs-alternatives-and-cuts.md) | L1 `synonyms`: runs of whole clauses, phrase alternatives, one step, and cuts in group order | proposed |
| [0040](./0040-l3-prefix-one-step-over-analyzed-terms.md) | L3 `prefix`: one step over the analyzed terms, the last one always, stacking fields exact | accepted (amended by 0046) |
| [0041](./0041-blob-chunk-size-floor.md) | A writer's blob chunk size has a 64 KiB floor; the encodings keep `1..=64 MiB` | accepted |
| [0042](./0042-l4-fuzzy-one-step-length-policy-and-corrections.md) | L4 `fuzzy`: one step over L3's terms, a length-based distance, and corrections of missing words | proposed (amended by 0046) |
| [0043](./0043-relaxed-response-states-the-hit-step.md) | A relaxed response states the text of its hit step, the last step that found a hit | proposed |
| [0044](./0044-l5-fallback-static-rank-then-index-recency.md) | L5 `fallback`: the collection's static rank, then index recency, flagged in the response | proposed (amended by 0047, 0048) |
| [0045](./0045-request-relax-options-and-cursor-settings.md) | A request's `relax` options: every key, merged one at a time, and cursors that record the settings | proposed (amended by 0047) |
| [0046](./0046-typo-and-exactness-tiers-as-cascade-steps.md) | Typo and exactness tiers are steps of the cascade, by the most edits any one word needs | proposed |
| [0047](./0047-filter-relaxation-as-a-last-step.md) | Filter relaxation (`filters: "last_resort"`) is one last step, L0 without its root filters | proposed (amended by 0048) |
| [0048](./0048-request-filter-kept-beside-the-compilation.md) | The request filter is kept beside the compiled query, which is replaced only through it | proposed |
| [0049](./0049-blob-pack-layout-and-durability.md) | Blob pack: layout, per-blob durable bind, recovery with roll-forward, pack generation | proposed |
| [0050](./0050-remote-blob-layout-transfers-resume-and-inline-blobs.md) | Remote blob layout: keys, parallel transfers, resume, inline small blobs, object times | accepted |
| [0051](./0051-blobs-with-documents-internal-api-pins-and-ordering.md) | Blobs with documents: the internal put_blob/put API, the pin set, and the blob-before-document order | accepted |
| [0052](./0052-blob-garbage-collection-mark-sweep-and-pack-rewrite.md) | Blob garbage collection: the mark, the sweep and the pack rewrite | accepted |
| [0053](./0053-blob-wiring-writer-proof-and-resume-api.md) | Blob wiring, writer proof and resume API: the pack opens read-write with the container's `WriterProof`, a read-only database opens it read-only, `verify` scans checksums by default, a resume token comes from `put_blob`'s error | accepted |
| [0054](./0054-blob-pack-reader-leases.md) | Reader leases on the local blob pack: one locked 64-byte lease per read-only handle in `<name>.persia.blobs.leases/`; GC does not sweep while a live lease is not at the committed seq | accepted |
