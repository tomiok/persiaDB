# ADR-0053: Blob wiring, the writer proof and the resume API

- Status: proposed
- Date: 2026-11-20
- Related: SPEC §4.8, §9.2, §9.3, §9.4, §9.5, §9.6, §10.1; ROADMAP 9.2.7, 9.3.4, 9.3.6, 9.4.5, 9.4.6, 9.5.3, 9.5.6,
  9.5.9, 13.2.6; ADR-0049, ADR-0050, ADR-0051, ADR-0052

## Context

Wave 3 of M9 wires the pack into the database open and finishes the remote resume. Left open by ADR-0049 to ADR-0052:
how the pack knows its opener is the container's writer (ROADMAP 9.2.7: two read-write `PackStore`s of one file must
not exist); what a read-only database does with the pack, and with no pack; whether `verify`'s checksum scan is on by
default and whether it covers the remote layout; how a caller learns a `ResumeToken`; what a store that cannot resume
does with one; and how a resumed upload escapes the sweep (ROADMAP 9.3.6: an upload older than the grace period,
bound after the sweep's manifest listing). The user decided (2026-11-20): a borrowed proof checked at open; a
read-only pack open that never creates the pack; the scan on by default, local only; the token from `put_blob`'s
error; `Unsupported` trait defaults; a restage that makes the chunks young.

## Decision

### The writer proof and the open (ROADMAP 9.2.7, 9.4.5)

1. **The proof.** A read-write pack open takes a `WriterProof<'_>`, which only `Container::writer_proof` mints, and
   only on a read-write handle (the one that took the writer lock); `Database::writer_proof` passes it through. It
   carries the container's `db_uuid`, which the pack's header must match, and borrows the handle. It is checked at
   open and not stored. `PackStore::open_writer` / `open_writer_in` are the only read-write opens; `open` / `open_in`
   are read-only and take a `db_uuid`. A second writer is impossible because its container open fails on the lock.
   The proof is the API gate; the pack's own OS lock, reusing the container's primitive (`ContainerFile::try_lock`,
   `Vfs::lock_beside`), is the enforcement. The proof is `Copy`, any read-write container mints one (a simulated one
   too), and an `Arc` clone of the store outlives the container, so the proof alone cannot enforce one writer. A
   read-write handle holds the lock from open to drop (Unix: the pack file itself; Windows: `<name>.persia.blobs.lock`),
   taken before the open sweeps or recovers anything; a created or rewritten pack file is locked before it is written,
   so the path never names an unlocked pack while its writer lives. A second read-write open fails with `PackLocked`.
   Remaining gap: before the pack file exists on Unix there is nothing to lock, so two read-write handles opened then
   can both exist; the one that loses the race to create the pack fails at its first write and is poisoned.
2. **Drop order.** The proof does not keep the container alive, so owners declare the pack before the container
   (`BlobDatabase`: pins, then blobs, then the log) and the pack drops first. Tests mint real proofs from a real
   `Container` over a `SimFile`; there is no test-only constructor.
3. **The open.** `BlobDatabase::create` / `open` open the database at `<path>` and its pack at `pack_path` of the
   container's canonical path, so a database opened through a symlink finds the pack beside the real file. The pack
   is still created by the first `put_blob` (SPEC §9.3).
4. **Read-only.** `ReadOnlyBlobDatabase` opens the pack read-only and never creates it. With no pack, a blob read
   fails as not bound. `refresh(reopened)` takes a newer read-only open of the documents first, then refreshes the
   pack, in that order because a blob is bound before any document that references it is logged. A reopened
   database of another `db_uuid`, at another canonical path, or older than the current open is refused and nothing
   changes. A lower manifest `seq` is older. Opens of one `seq` are ordered by (replayed WAL records, `wal_end`):
   more of both, or both equal, is taken; fewer of both is `RefreshedOlder`; any mix is `RefreshDiverged` (SPEC
   §9.3). Both are refused and nothing changes. The pack may be newer than the documents (the GC window, ROADMAP
   9.2.16): `BlobNotFound` for a swept blob of a deleted document. `PackReplaced` (a GC rewrite) reopens the pack
   read-only at the same path with the same `db_uuid`; the refresh holds `&mut self`, so no stream still borrows the
   old pack. Any other pack error, and a reopen that fails, restores the previous documents so the documents and the
   pack stay paired. `PackStore::refresh`
   itself does not reopen or remap (ROADMAP 9.2.13). The engine's `ReadOnlyDatabase` has no refresh of its own, so
   the caller reopens it.

### `verify` (ROADMAP 9.5.3, 9.5.6)

5. **References.** `verify` checks every reference `ReadOnlyDatabase::blob_references` lists: each live document of a
   committed segment and every put of the replayed WAL. A missing blob (no pack, not bound, or a manifest that fails
   `check_ref`) is an error naming the collection, document id and `blob_id`. Integrity failures exit with the
   corruption status 3. The CLI reaches the checks through `persia::internal::verify`, so its dependencies do not change.
6. **The scan.** By default `verify` also scans the committed frames `12288..log_end`: every frame's checksum and
   every chunk's CRC32C, orphans included (counted, never errors). Damaged chunk bytes are otherwise found only when
   someone reads them. `--quick` skips the scan. `--deep` also re-hashes every referenced blob with SHA-256, the only
   check that catches CRC-valid bytes under the wrong id. Only the local layout is covered; the remote one is
   ROADMAP 13.2.6.

### Resumed uploads (ROADMAP 9.3.4, 9.3.6)

7. **The token comes from the error.** A `put_blob` on a resumable store that fails after its upload staged chunks
   returns `Error::UploadInterrupted { token, source }`. Wrapped: a reader error, a staging, restaging or listing error,
   and a bind's backend error whose outcome is known. Never wrapped: an unknown bind outcome (the blob may still be
   bound), `BlobIdMismatch`, a caller's error, or a small (inline) blob. The pins drop with the error, and GC's grace
   period keeps the upload resumable (SPEC §9.5). A resume that fails again returns the token of the upload it was
   staging (after a conflict restage, the new one). `UploadInterrupted` is not corruption: `is_corruption` is false
   and `corrupt_offset` is `None`, even when `source` is corruption. The source carries the cause.
8. **The caller passes the whole blob.** `PutBlobOptions::resume` takes a reader of the whole blob from its start.
   `put_blob` pins the old upload before listing it, reads past the kept run without uploading it, and checks each
   kept chunk against the staged chunk's CRC32C (a reader that differs there is `InvalidResume`, a caller's error).
   A staged chunk whose read reports corruption is not part of that run: the run ends there instead of failing the
   resume. The object stays, so staging the index conflicts when the stored bytes differ and the upload restarts under
   a new id. The listing is sorted by index first; a repeated index whose lengths differ ends the run before it.
   Amended (ROADMAP 9.3.7): a resume also carries the whole blob's `expected_id` or `expected_size`
   (`ResumeUnchecked` otherwise, before anything is read).
   It pins a new upload before any chunk is copied to it. The token's chunk size wins over `chunk_size`, because chunk
   objects do not record their size, and it is checked against the floor before anything is read. A network client
   that wants to send only the rest needs an offset query, which M11 adds if it needs one.
   The token is not checked against the store (decided 2026-11-21: kept as is, documented). It holds only the upload
   id and the chunk size, so an unknown upload id and a token of another prefix or bucket are one case: an empty
   listing starts the blob from chunk 0 under the token's id (an empty blob writes no chunk, so item 10 binds it under
   a new id). Chunks there that the resume does not keep and would
   write other bytes over conflict, and the upload restarts under a new id; GC takes the old chunks. A token of
   another chunk size than its upload's is such a case: its chunk 0 is not of the token's length (unless the blob
   is that one short chunk, which is then kept), so the kept run is empty and the first staged index the resume
   writes conflicts (chunk 0 itself may have been swept). Full chunks of the token's size that are not the reader's
   bytes are `InvalidResume`, with nothing staged. The blob id is always computed from the bytes bound.
9. **Stores that cannot resume.** `BlobStore::resumable`, `resume` and `restage` default to `false` and
   `ResumeUnsupported`; only `RemoteBlobStore` overrides them (SPEC §9.3: local uploads are not resumable). A resume on
   such a store fails before reading, and such a store never returns `UploadInterrupted`.
10. **The 9.3.6 restage.** A resume that writes no chunk its listing did not show copies the chunks before its last
    window to a new, pinned upload, stages the last window there, and binds that upload. The sweep counts an upload's
    age from its newest chunk, and re-staging the same bytes writes nothing. Without this step the bound upload would
    keep its old age, and a sweep whose manifest listing came before the bind could delete it. This replaces the
    alternative of `pin_contained` checking chunk 0, which cannot help once the pins have dropped before the chunk
    phase.
    The cost is a re-read and re-upload of every chunk before the last window when the upload was fully staged; it is
    accepted rather than skipping the copy for uploads that look young, because the client's clock and the backend's
    need not agree.
11. **No discard of the token's upload.** A resume never discards the token's upload: a replayed token may name an
    upload bound to another blob. It discards only an upload it restaged to; GC decides about the token's.

## Consequences

- Every pack test needs a real container to mint a proof. `PackStore::open(.., ReadWrite, ..)` no longer exists.
- `persia verify` reads the whole pack by default; `--quick` restores the old cost.
- A resume re-reads the kept run from the caller (not re-uploaded). A resumed blob's upload id may differ from the token's.
- Remaining race (ROADMAP 9.5.10, added on `blob/resume` and merged after this PR): the remote sweep re-lists an
  upload's chunks under the pin set's lock before deleting them, and keeps the upload when a chunk is younger than
  the grace period or was not in its first listing. A resume that pins, lists, stages a new chunk under the old
  upload, binds and drops its pins between the sweep's upload listing and its locked check, and is referenced later
  by `BlobRef`, can lose the old chunks. Without `strong_list` the first listing can also miss a fresh chunk.
  Closed by ROADMAP 9.5.10: the sweep re-lists an expired upload under the pin set's lock and keeps it when the
  listing changed.

## Alternatives considered

- An owned proof that shares the lock (`Arc` of the locked file), stored in the pack: stronger, but it changes
  `Container`'s unlock-on-drop path, which is crash-safety code.
- A separate `begin_upload` call that returns the token before streaming: it survives a process crash, but the API is
  larger. With the error-carried token, a crashed process's upload stays resumable only if the caller recorded the
  token.
- `verify`'s scan behind a flag: cheaper by default, but damaged bytes stay hidden until a read.
- Always restaging on a resume: it closes the 9.5.10 race too, but uploads the kept run again.
