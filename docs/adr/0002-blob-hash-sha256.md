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
- Persia always computes the hash itself while chunking (ROADMAP 9.1.2). A caller-supplied id is only an expected
  value; a mismatch fails the upload. Dedup therefore never trusts the client.
- Per-chunk integrity stays CRC32C (SPEC §9.2): it detects storage corruption cheaply on range reads, and the
  SHA-256 covers the whole blob.
- Add `sha2` to the allowed dependencies (library tier). Its `asm`/SHA-NI acceleration is a feature flag that can
  be turned on later behind a benchmark.

## Consequences

- Deduplication is safe across untrusted clients, and `verify` can prove a blob matches its id.
- Hashing costs roughly 1.5–2 GB/s per core with SHA extensions, and several hundred MB/s without them. That is
  well below network and disk speed for typical uploads, and it runs once per upload, not on reads.
- Ids are 32 bytes instead of 16, which is negligible in the `BlobRefs` column.
- The remote layout `blobs/<hex[0..2]>/<blob_id>/…` (SPEC §9.4) uses the 64-character hex form.

## Alternatives considered

- **xxh3-128:** fastest and already a dependency, but unsafe for dedup with untrusted input. Rejected for ids;
  it remains the shard-routing and internal-hash choice (SPEC §11.1).
- **BLAKE3:** faster than SHA-256 in software and cryptographically strong, but it would add a dependency, and
  SHA-256 is what S3, GCS and Azure checksums and most client tooling already speak.
- **Trust the client's hash:** zero server cost, but it reintroduces the aliasing attack.
