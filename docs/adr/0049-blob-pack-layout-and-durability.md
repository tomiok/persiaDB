# ADR-0049: The blob pack file: layout, per-blob durability, recovery and reads

- Status: proposed
- Date: 2026-11-18
- Related: SPEC §4.2–§4.8, §9.2, §9.3, §9.5, §17, §18.9, §18.10; ROADMAP 9.2.1–9.2.5, 9.5.2, 9.5.3; ADR-0007,
  ADR-0008, ADR-0009, ADR-0010, ADR-0013, ADR-0016, ADR-0017, ADR-0034, ADR-0035

## Context

SPEC §9.3 sketches the local blob store: a file `<name>.persia.blobs` beside the container, holding a header,
append-only `BlobChunk` frames `{upload_id, chunk_idx, len, crc, bytes}` staged under an upload id (ADR-0034),
`BlobIndex` frames holding blob manifests (ADR-0035) with the chunk frames' offsets, and a superblock "like SPEC §4.3".
It leaves open the header's magic and version, the superblock's fields, the frame payloads, how the index is found
at open, what a torn tail is, how a reader shares the file with the writer, locking, and when a blob is durable
(§18.9). The user decided (2026-11-18): a blob is synced per bind, and the pack's superblock carries a `u64`
generation from v1 on so a reader can tell a rewritten pack from the one it opened (§18.10 (a)).

The container (SPEC §4) already has every primitive the pack needs: a CRC-checked header page, double-buffered
4096-byte slots, `PFR1` frames with a `frame_seq` run and bounded-memory scanning, a crash-simulating file
(`SimFile`, `SimFs`), and mapped reads. What it has that the pack does not need is its *semantics*: a Manifest frame
named by the slot, a WAL range, frames after `log_end` that may hold acknowledged writes (and so quarantine, ADR-0014).

## Decision

1. **Header.** The container header's layout and validation order (SPEC §4.2, ADR-0007) with the magic
   `"PERSIAB\x01"`, so neither file opens as the other. `format_major = 1`, `format_minor = 0`; `db_uuid` is the
   database's (the container's) at creation, and an open by another database fails (`PackOfAnotherDatabase`): a GC
   of a foreign pack would delete blobs its own documents reference. `page_size` 4096 and advisory. The flags follow
   §4.2's rules in **their own namespace**: `persia_format::header::HeaderRules` names a file kind's magic and known
   bits (`CONTAINER`, `BLOB_PACK`), so a bit assigned to the container is never "known" in a pack. No pack bit is
   assigned. A read-write open of a pack with an unknown `rocompat_flags` bit fails (`PackReadOnly`).
2. **Superblock.** Two 4096-byte slots at 4096 and 8192, chosen by the table of SPEC §4.3
   (`persia_format::superblock::pick_active`, shared with the container): `seq`, `index_off`, `index_len` (the last
   committed `BlobIndex` frame, 0/0 when none), `log_end`, `generation`, `blob_count` (the `BlobIndex` frames in
   `12288..log_end`), `next_frame_seq`, `pack_id u128`, zero reserved bytes, CRC32C at 4092. A new pack has `seq 0`,
   `log_end 12288`, `generation 1`, `blob_count 0`, `next_frame_seq 1` and a random `pack_id` in both slots. Bounds
   (checked before use) admit only states a writer produces: `log_end` 8-aligned in `12288..=file length`;
   `generation ≥ 1`; `next_frame_seq ≥ 1`; `seq 0` exactly the new-pack values (any generation and `pack_id`: what a
   rewrite with no live blob writes); `seq ≥ 1` with `1 ≤ blob_count ≤ (log_end − 12288) / 120` (120 bytes is the
   shortest index frame, an empty blob's), the index frame 8-aligned, at least 120 bytes, and ending exactly at
   `log_end` (a bind appends it last), and `next_frame_seq ≥ 2`.
3. **Generation and pack id** (§18.10 (a)). A new pack is generation 1 with a random `pack_id`. A pack rewrite
   (ROADMAP 9.5.2, not built here) writes the old generation plus one and a new `pack_id` into both slots of the new
   file. A handle compares `(pack_id, generation)` at the path with the ones it opened (`PackStore::refresh`):
   another value means the path names another file, and the handle must reopen (`Error::PackReplaced`). The
   generation orders rewrites; the id tells apart files the generation cannot (a pack deleted and recreated, a
   restored copy). Together they stand in for the file identity Windows lacks (ADR-0016 item 5).
4. **Frames** are SPEC §4.4 frames, numbered from 1 in one contiguous `frame_seq` run, never compressed (blobs are
   stored as they are, §9.2; a compressed pack frame is corruption). Kind **6 `BlobChunk`**, payload:
   `upload_id u128, chunk_idx u32, len u32 (1..=64 MiB), crc u32 (CRC32C of the bytes), reserved u32 = 0, bytes`,
   exactly `32 + len` bytes. Kind **7 `BlobIndex`**, payload: `manifest_len u32, reserved u32 = 0`, the blob
   manifest (SPEC §9.1.1, its own trailer included), zeros to a multiple of 8, then one `u64` per chunk, in chunk
   order: the offset of the `BlobChunk` frame holding it, 8-aligned, at or after 12288 and before the index frame.
   A container reader skips kinds 6 and 7 as it skipped unknown kinds (its replay and recovery look only at
   `WalChunk` and `Manifest` frames), so no container changes behavior; `persia container` names them.
5. **Visibility.** A blob is bound when its `BlobIndex` frame lies in `12288..log_end` of the active slot. Chunk
   frames that no committed index names are staging or orphans (SPEC §9.3). Nothing after the `log_end` of the
   newest slot written was ever acknowledged: `stage_chunk` promises nothing durable, and `bind` answers `Bound` only
   after the slot that covers the index frame is synced. The rule that a binding is shown only once durable
   (`AlreadyBound`, `manifest`, `contains`; the `BlobStore` contract) binds the **writer handle**, which publishes an
   entry only after its slot sync returns. A read-only handle in another process reads the slots from the file and may
   see a binding between the slot write and its sync; that is harmless, because a read-only handle logs no document,
   and a crash that reverts the slot only makes it see less after a reopen.
6. **Bind is synced per blob** (§18.9 (a)). A writer mutex serializes every write; `bind` holds it from start to
   publish. It checks the id is not bound (`AlreadyBound` otherwise, nothing written) and that the upload has bound no
   other blob (`InvalidManifest`: an upload binds one blob, and a second index naming it would make every later open
   fail), checks that this handle staged every chunk the manifest names with its length and CRC32C
   (`ChunkNotStaged` otherwise), appends the `BlobIndex` frame after them, syncs the file's data, writes the inactive
   slot (`seq + 1`, the new index frame, `log_end` = the end of the file's frames, `blob_count + 1`, the same
   generation and pack id) and syncs again, then publishes under the `RwLock`'s write side. The syncs run under its
   read side only, so readers are not blocked by them. A blob is therefore durable before any document referencing
   it can be logged, whatever the WAL's durability (§18.9 (b) is moot), and the pack's index covers a blob before
   any container byte references it (the order a live copy relies on, §4.8). A failed write or sync poisons the
   handle (SPEC §4.5): before the slot write the blob is not bound (an I/O error, then `PackPoisoned`); from the slot
   write on its outcome is unknown (`BindOutcomeUnknown`), and every later `bind` of that id on the handle answers
   `BindOutcomeUnknown` too, never "not bound". The outcome is learned by **reopening** the pack: the next
   read-write open rolls the bind forward (item 9) or finds it never reached the disk. Either way no document was
   logged, so the worst case is an orphan. Like the container, the pack relies on a failed `fdatasync` being reported
   to the handle that wrote (an error reported to another descriptor of the file, as Linux's errseq allows, is not
   seen): the same limit as SPEC §4.5.2.
7. **Lazy, durable creation** (§9.3, §18.9 (c)). The first `stage_chunk` or `bind` of a database creates the pack:
   header and both new-pack slots written to `<name>.persia.blobs.tmp.<nonce>.<n>`, synced, hard-linked to the pack
   name (which fails if it exists), then, whatever happened since the temporary file was created, the temporary name
   removed and the directory synced (as `Container::create_in` does), all before the first chunk is appended. A crash
   leaves no pack or a complete empty one (and possibly a stale temporary file); a failure the process survives removes
   the temporary name unless that removal fails too, which leaves a stale temporary file for the sweep (ROADMAP 9.2.6). A failure after the link, or a link that finds the name taken (a second writer), stops the
   handle; a reopen finds what the path holds. The pack's directory sync is its own: the pack is not created with the
   container.
8. **Locking.** The pack takes no lock: only the container's writer, holding the container's writer lock (or, on
   Windows, `<name>.lock`), opens it read-write (SPEC §9.5: GC and `put_blob` run in that process). Opening the pack
   read-write without holding the container's lock is outside the contract: a second writer would truncate the
   first one's staged tail. Nothing enforces it yet (a proof-of-lock argument from the container is an open question).
9. **Open and recovery: the index is rebuilt by a scan.** An open validates the header, picks the active slot,
   checks it against a file length taken after reading the slots, then walks the frames of `12288..log_end` by
   their headers: each frame must have the magic, a length that fits before `log_end`, and the next `frame_seq` of
   one run starting at 1 and ending at `next_frame_seq - 1`. Only kinds 6 and 7 may appear: any other kind below
   `log_end` (unknown, Padding, a container's) is corruption, since no pack writer emits one and a new kind would
   come with a pack `incompat_flags` bit. `BlobChunk` frames are only located (their payloads are checked when
   read); every `BlobIndex` frame is read whole and validated (frame CRC, payload, manifest, distinct offsets that
   are `BlobChunk` frames the walk found before it), its id and upload must not be bound twice, their number must be
   `blob_count`, and the last one must be the slot's `index_off`/`index_len`. Any violation below `log_end` is
   corruption: those frames were committed. Frame headers have no CRC of their own, so a damaged `payload_len` below
   `log_end` misaligns the walk and the open fails (`verify` and a rewrite from the index would be the repair; none
   is built). Memory is one offset per chunk and one entry per blob; the open reads 24 bytes per chunk frame and
   every index frame, not the chunks' bytes.
   **After `log_end`**, a read-write open first **rolls forward**: it scans the tail with full validation (every
   frame's CRC, in bounded memory), continuing the `frame_seq` run, and keeps the longest prefix that ends in a
   `BlobIndex` frame whose chunks are all in the pack, with a new id and upload. That is a bind cut after its data
   sync and before or during its slot write (its blob becomes an orphan, harmless), or a bind whose synced slot was
   damaged since (its blob was acknowledged and documents may reference it: a torn slot write and a rotted slot look
   the same, which is why the container quarantines, ADR-0014). The kept frames are synced and committed by a new
   slot (`PackRecovery::rolled_forward`). Everything after them (a torn frame, staged chunks of a process that died,
   garbage) was never acknowledged (item 5) and is truncated, then synced, so the next append can never land behind
   stale frames (SPEC §4.4). A read-only open never writes, and ignores the tail; it sees a rolled-forward blob once
   the writer has opened. No quarantine copy: what is truncated was never acknowledged, unless both the newest slot
   and the frames of its bind are damaged, a double fault a single-copy file cannot survive (the blob is then lost, and
   `verify` reports the documents that reference it).
10. **Readers and the writer share the file as the container does** (SPEC §4.5.1, §4.7). The writer appends only
    after its `log_end` and writes slots in place; recovery truncates only after `log_end`; a rewrite (9.5.2) will
    write a new file and rename it over the pack. A read-only handle holds the snapshot of its open and reads frames
    below its `log_end`; `refresh` re-reads the slots and walks only the frames between the old and the new
    `log_end` (or reports `PackReplaced`).
11. **Reads.** `manifest(id)` reads the id's `BlobIndex` frame again and decodes it on every call (checked with
    `check_blob_id`), so a damaged frame is never served from memory; `read_chunk` reads the `BlobChunk` frame at the
    offset the index named, validates it, and checks its `upload_id`, `chunk_idx` and stored CRC against the
    manifest; `read_chunk_checked` then checks length and CRC32C (SPEC §9.3). Chunks of this handle's staged,
    unbound uploads are found by `(upload_id, chunk_idx)`.
12. **Mapped reads** (SPEC §4.7, ADR-0017) are opt-in through a `persia_storage::container::MapPermit`, which only an
    `unsafe` call creates, because `persia-blob` forbids `unsafe`. Its contract is SPEC §4.7's for the pack, plus: at
    most one read-write `PackStore` of the pack open at a time, anywhere, until the pack enforces a single writer
    (item 8). `MapPermit::map` refuses a range starting below 12288 (the header and slots are rewritten in place).
    The pack then maps `12288..log_end` once and again after each bind, and reads committed frames from the mapping;
    others are read with `pread`. A failed remap keeps the previous mapping. Tests and benches cannot create a permit
    outside xtask's `UNSAFE_ALLOWED` files, so the real mapping is covered by a doctest and the window logic by unit
    tests over a copying stand-in.

## Consequences

- On Windows a live mapping keeps a file from being shrunk (SPEC §4.7): a read-write open that must truncate a
  pack tail fails with an I/O error while a reader maps the pack, as for the container.
- One `fdatasync` pair per blob: a database ingesting many tiny blobs is bounded by sync latency (milliseconds on
  macOS). Batching syncs would need the WAL and the container commit to sync the pack first (§18.9's alternative);
  not now.
- Opening a pack costs one header read per frame below `log_end` and one read per blob; it is not proportional to
  the blob bytes. Very many small blobs make open slower; an index checkpoint frame could fix that later, behind an
  `incompat_flags` bit.
- Staged chunks of a crashed process, and duplicates' chunks, stay in the pack below `log_end` until the pack
  rewrite reclaims them (SPEC §9.2); a local upload does not survive a reopen of the pack (local uploads are not
  resumable; resume is remote, §9.4).
- GC and `verify` (ROADMAP 9.5) reuse `pack::layout` (every codec), the open walk (the bound set and each chunk's
  offset), and `PackStore::read_chunk`. The rewrite copies the live blobs' chunk frames (renumbered from 1, offsets
  re-recorded in new `BlobIndex` frames) into a new file with generation + 1, then renames it over the pack as
  §18.10 proposes. `verify` re-hashes each blob and checks every chunk frame's CRC, which the open does not.
- A pack beside another database's container is refused at open (item 1); the caller passes the container's
  `db_uuid`.
- The rewrite copies into a new file with generation + 1 and a new `pack_id`.

## Alternatives considered

- **Reuse `Container` as is**, a pack being a container with `BlobIndex` frames in the Manifest's place: the slot
  would have to name a Manifest frame, the WAL range and the quarantine of acknowledged tail frames would not apply,
  and the container's magic would let a container open open a pack. The pack reuses the container's codecs, slot
  selection, file abstraction and simulators instead, and keeps its own small recovery.
- **A chain of index frames** (each `BlobIndex` naming the previous): open reads only index frames, but recovery
  could not cross-check chunk offsets against the frames, and the ROADMAP asks for a rebuild by scan.
- **Full validation of every frame at open**: reads every blob byte at each open; `verify` does it on request.
- **Truncate everything after `log_end`** (no roll-forward): simpler, but a damaged newest slot would then destroy
  an acknowledged blob that documents reference. **Keep the tail uncommitted** (the container's rule): a later
  commit would publish a cut bind's index frame behind the writer's back, out of `blob_count`'s count.
- **Sync per batch** (§18.9 (a)'s alternative): fewer syncs, but the WAL and the container commit must then sync the
  pack before themselves; the user chose per blob.
