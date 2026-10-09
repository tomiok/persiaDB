# ADR-0002: Blobs are identified by SHA-256

- Status: accepted
- Date: 2026-10-09
- Related: SPEC §9.1, §9.2, §18.1; ROADMAP 0.4.3, 1.10.1, 9.1.1

## Context

Blobs are content-addressed (SPEC §9.2): `blob_id` is a hash of the bytes, which gives deduplication and
idempotent `put` for free. Two blobs with the same id are treated as the same blob, so the hash decides
whether one client's upload can replace or alias another's.

In server mode, blob bytes come from clients we do not control. With a non-cryptographic hash, a malicious client
can construct two different files with the same id and, by uploading one first, make documents that reference
the other serve the wrong bytes. That breaks the guarantee that a referenced blob's content is exactly what was
stored.

Candidates: xxh3-128 (already a dependency, about 10x faster, not collision resistant against an adversary) and
SHA-256 (collision resistant, hardware-accelerated on modern x86-64 and ARMv8 CPUs).

## Decision

- `blob_id` = SHA-256 of the blob's full byte content: 32 bytes on disk, lowercase hex in text (keys, JSON, logs).
- Persia always computes the hash itself while chunking (ROADMAP 9.1.2). If a caller supplies an expected id, it
  is only checked against the computed one; a mismatch fails the upload. Dedup therefore never trusts the client.
  Because the id is known only after the last byte, chunks must not become addressable under a `blob_id` before
  it is verified; how uploads are staged is SPEC §18.8.
- Per-chunk integrity stays CRC32C (SPEC §9.2): it detects storage corruption cheaply on range reads, and the
  SHA-256 covers the whole blob.
- Add `sha2` to the allowed dependencies (library tier). It auto-detects hardware backends (x86 SHA-NI, aarch64
  SHA2) at runtime by default.

## Consequences

- Deduplication is safe across untrusted clients, and `verify` can prove a blob matches its id.
- Hashing runs once per upload, never on reads. Approximate throughput: 1.5–2 GB/s per core with SHA extensions,
  which is faster than typical network ingest; several hundred MB/s without them, which can bound upload
  throughput on fast networks or NVMe. To be confirmed by a benchmark (ROADMAP 9.1.2).
- Ids are 32 bytes instead of 16, which is negligible in the `BlobRefs` column.
- The remote layout `blobs/<hex[0..2]>/<blob_id>/…` (SPEC §9.4) uses the 64-character hex form.

## Alternatives considered

- **xxh3-128:** an order of magnitude or more faster, and already a dependency, but unsafe for dedup with untrusted input. Rejected for ids;
  it remains the shard-routing and internal-hash choice (SPEC §11.1).
- **BLAKE3:** faster than SHA-256 in software and cryptographically strong, but it would add a dependency, and
  SHA-256 is what most client tooling (`sha256sum`, language standard libraries) already speaks.
- **Trust the client's hash:** zero server cost, but it reintroduces the aliasing attack.
