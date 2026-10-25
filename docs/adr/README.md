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
| [0018](./0018-embedded-sdk-bindings.md) | Embedded Go and TypeScript bindings share one C ABI | proposed |
| [0019](./0019-sweeping-temporary-files.md) | Stale temporary files are swept under their lock, on request only | proposed |
| [0020](./0020-cli-depends-on-storage.md) | The `persia` CLI may depend on `persia-storage` directly | proposed |
| [0021](./0021-os-calls-for-file-system-type-and-identity.md) | `libc` and `windows-sys` for the file system type and the Windows file identity | proposed |
| [0022](./0022-schema-model-and-hash.md) | Collection schema model, strict JSON and the schema hash | proposed |
| [0023](./0023-document-model-and-encoding.md) | Document model, canonical floats and the CBOR document encoding | proposed |
| [0024](./0024-manifest-encoding-and-commit-diffs.md) | Manifest encoding and commits as diffs | proposed |
