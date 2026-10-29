# Persia DB — Specification

Status: **Draft v0.1** · Format version: **1.0 (unstable until v1.0 release)**

Keywords: MUST, SHOULD, MAY follow RFC 2119.

---

## 1. Goals and non-goals

### 1.1 Goals
- Embedded, single-file search database ("SQLite for search") with an optional server mode.
- Full-text search with BM25F ranking, filters, facets, and **zero-results avoidance by default**.
- Storable in the local filesystem **or** object storage (S3, GCS, Azure Blob) using the same segment bytes.
- Horizontal scalability: stateless query nodes, immutable segments, shard-per-writer.
- Content-addressed blob storage for client files (pdf, jpg, mp4...) referenced from documents.
- First-class SDKs: the embedded Rust API; embedded Go and TypeScript bindings (§13.1, ADR-0018); network
  clients in Rust and Go (§13.2).
- Crash-safe: a power loss at any instruction never corrupts committed data.

### 1.2 Non-goals (v1)
- Text extraction, OCR, transcription (clients supply metadata/text; blobs are opaque).
- Vector / semantic search (planned post-v1, format reserves feature flags).
- SQL, joins, multi-document transactions, cross-collection queries.
- Multi-writer on the same shard (single writer per shard, enforced by lease).
- Encryption at rest inside the format (delegated to storage layer / disk in v1).

---

## 2. Terminology

| Term | Meaning |
|---|---|
| **Database** | One logical Persia DB: a set of collections + blobs. Opened via path or URL. |
| **Collection** | Named set of documents sharing a schema, analyzers and ranking config. |
| **Document** | A record: external `id` (string), typed fields, optional `blob_ref`. |
| **Segment** | Immutable, self-contained index unit for one collection (SPEC §5). |
| **Memtable** | In-memory mutable buffer of recent writes; searchable (near-real-time). |
| **WAL** | Write-ahead log of operations not yet flushed to a segment. |
| **Manifest** | Versioned snapshot listing live segments per collection + schemas (SPEC §8.7). |
| **Commit** | Atomic publication of a new manifest. |
| **Frame** | Checksummed record in the single-file container (SPEC §4). |
| **Blob** | Opaque client file, content-addressed, stored by chunks (SPEC §9). |
| **Shard** | An independent Database that owns a hash range of document ids (SPEC §11). |
| **Relaxation** | Controlled loosening of a query to avoid empty results (SPEC §7.6). |

---

## 3. Architecture overview

```
 ┌───────────────── persia (embedded facade) ─────────────────┐
 │  Db ── Collection ── put/get/delete/search/blob             │
 ├──────────────┬──────────────┬───────────────┬──────────────┤
 │ persia-engine│ persia-blob  │persia-analysis│              │
 │ memtable/WAL │ chunks/dedup │ analyzers     │              │
 │ segments     │              │               │              │
 │ query+rank   │              │               │              │
 ├──────────────┴──────────────┴───────────────┴──────────────┤
 │ persia-storage:  Storage trait                              │
 │   memory │ local file container │ s3 │ gcs │ azure          │
 ├─────────────────────────────────────────────────────────────┤
 │ persia-format: encodings, frames, superblock, checksums     │
 └─────────────────────────────────────────────────────────────┘
      ▲ persia-server (gRPC/HTTP) embeds the same engine ◀── network SDKs (§13.2)
      ▲ embedded bindings (Go via cgo, TypeScript via N-API) call persia through persia-ffi (§13.1, ADR-0018)
```

Principles:
1. **Immutable segments** + **append-only metadata** ⇒ trivial crash recovery, caching and replication.
2. **Logical objects**: Segment, Manifest, WalChunk, Blob. Backends decide physical layout
   (single file locally, one object per entity remotely). Segment bytes are identical in both.
3. **Engine is synchronous and storage-agnostic**; async exists only at the cloud I/O boundary and server.

---

## 4. Local file container

A local database is two files:
- `<name>.persia` — index container (header + frames).
- `<name>.persia.blobs` — blob pack file (SPEC §9.3).

### 4.1 Conventions
- All integers little-endian. All structures 8-byte aligned, except inside a block-framed section (SPEC §5.2), whose
  blocks and block table are byte-aligned (only the section start is aligned). Offsets are absolute `u64`, except inside a segment, whose offsets are relative to its first byte (§5.1).
- Checksum: **CRC32C** for headers/frames; **xxh3-64** (or xxh3-128) for internal content hashes and routing where
  noted, always with the default secret and seed 0: persisted hashes depend on it, so changing it is a format change.
  Blob ids are **SHA-256** (SPEC §9.1, ADR-0002).
- Compression: **zstd** (level configurable); every compressed unit declares its codec.
- Varints: unsigned LEB128, least significant 7-bit group first, high bit = "more bytes follow". Only the shortest
  encoding is valid; an overlong encoding, or a value wider than its declared type (`u32`: at most 5 bytes, the 5th
  holding 4 bits; `u64`: at most 10 bytes, the 10th exactly `0x01`), is corruption.
- Zigzag: a signed integer `n` is stored as the unsigned `(n << 1) ^ (n >> (bits - 1))` (arithmetic shift), i.e.
  `0, -1, 1, -2, ...` → `0, 1, 2, 3, ...`; a signed varint is the varint of its zigzag value.

### 4.2 Header (first 12288 bytes: header page + two superblock pages)
```
0      8   magic          "PERSIA\0\1"
8      2   format_major   u16   (reader MUST reject unknown major)
10     2   format_minor   u16
12     4   header_crc     u32   (over bytes 0..12 + 16..4096)
16     8   incompat_flags u64   (reader MUST reject unknown bits)
24     8   rocompat_flags u64   (unknown bits => open read-only)
32    16   db_uuid
48     4   page_size      u32   (default 4096)
52  4044   reserved       MUST be zero
4096  4096  superblock slot A (see 4.3)
8192  4096  superblock slot B
12288       first frame (SPEC §4.4)
```
The header and each slot occupy their own 4096-byte page (ADR-0008), so a torn or unreadable sector damages at most
one of the three; 4096 bytes is the largest physical sector in common use. These offsets are fixed: they do not
depend on `page_size`.

Validation (ADR-0007), in this order, so a newer format is never misreported as corruption:
1. `magic` differs ⇒ corruption (not a Persia container).
2. `format_major` ≠ 1 (the only major so far) ⇒ *unsupported*, not corruption: a future major may move every other
   field, `header_crc` included, so nothing past `format_major` is interpreted. Any `format_minor` is accepted.
3. `header_crc` mismatch ⇒ corruption.
4. Any `incompat_flags` bit this reader does not know ⇒ *unsupported*. No bits are assigned yet.
5. `page_size` not a power of two in `4096..=65536`, or non-zero `reserved` bytes ⇒ corruption.
6. Any `rocompat_flags` bit this reader does not know ⇒ the container opens **read-only**. No bits are assigned yet.

A writer sets only bits it knows, and records `format_minor = max(the container's minor, its own minor)` when it
creates or rewrites the container (SPEC §4.6): minors are additive (SPEC §15), so the field names the newest minor
that has created or rewritten the container and never decreases (ADR-0008). The header page is never written in
place, since it has no second copy (ADR-0015): a read-write open by a newer build leaves `format_minor` as it is
until the next rewrite, so the field is a lower bound on the minors that have written the container.

`page_size` is advisory (ADR-0008): the alignment a writer may give Segment frame payloads, using Padding frames
(SPEC §4.4), so a mapped segment starts on a page boundary. No offset a reader computes depends on it, and a
container may be rewritten with a different value.

### 4.3 Superblock (double-buffered, LMDB-style)
Each slot is one 4096-byte page (slot A at 4096, slot B at 8192; ADR-0008):
```
0   8  seq            u64  (monotonic commit counter)
8   8  manifest_off   u64  (offset of the Manifest frame)
16  8  manifest_len   u64
24  8  log_end        u64  (end of last durable frame)
32  8  wal_start      u64  (first WAL frame still needed)
40  8  fence_token    u64  (writer fencing, SPEC §11.3)
48  8  next_frame_seq u64  (frame_seq of the frame at log_end, SPEC §4.4)
56  ..  reserved      MUST be zero
4092 4 crc            u32  (CRC32C over bytes 0..4092 of the slot)
```
A slot is *valid* when its CRC matches and its reserved bytes are zero; a never-written (all-zero) slot is invalid.
A new container has no frames yet: both slots hold `seq = 0`, `log_end = wal_start = 12288` (the first frame
offset), `next_frame_seq = 1` and zero manifest fields, so recovery (SPEC §4.4) scans from 12288 (ADR-0008).
Slot decoding does not check the offsets; the container reader checks the active slot against the file before
using any of them, and any violation is corruption:
- `wal_start` and `log_end` are multiples of 8 with `12288 ≤ wal_start ≤ log_end ≤` file length;
- `next_frame_seq ≥ 1`, and `≥ 2` after a commit (a commit has at least its Manifest frame);
- `seq = 0` (never committed) holds exactly the new-container values above for `log_end`, `next_frame_seq` and the
  manifest fields;
- otherwise the Manifest frame lies in `12288..log_end` (`manifest_off` and `manifest_len` multiples of 8,
  `manifest_len ≥ 32`), and the frame there is a valid Manifest frame of exactly `manifest_len` bytes.

Open: read both slots, discard invalid ones, use the one with the highest `seq` (ADR-0007):

| Slot A  | Slot B  | Active                                                                       |
|---------|---------|------------------------------------------------------------------------------|
| valid   | valid   | higher `seq`; equal `seq` with identical bytes ⇒ A; equal `seq` with different contents ⇒ corruption |
| valid   | invalid | A                                                                            |
| invalid | valid   | B                                                                            |
| invalid | invalid | corruption (no valid superblock)                                             |

Two different commits can never share a `seq` (each writes `seq+1` into the inactive slot), so equal `seq` with
different contents means one slot is wrong and there is no way to tell which. Commit alternates slots: the next
commit writes the slot that is not active.

### 4.4 Frames
From byte 12288 (after slot B) the file is an append-only sequence of frames:
```
0   4  frame_magic   bytes "PFR1" (0x50 0x46 0x52 0x31)
4   1  kind          1=WalChunk 2=Segment 3=Manifest 4=Padding 5=TombstoneSidecar
5   3  flags         (bit0: zstd-compressed payload)
8   8  payload_len   u64  (unpadded length)
16  8  frame_seq     u64
24  ..  payload       zero-padded to the next multiple of 8 bytes
..  4  crc32c        u32  over bytes 4..(24 + padded payload), i.e. kind..padding
..  4  reserved      u32  MUST be zero
```
Total frame length is `24 + pad8(payload_len) + 8`, so every frame starts 8-byte aligned (SPEC §4.1).
`flags` is a 24-bit little-endian integer.

`frame_seq` numbers the frames of a container file in append order (ADR-0013): each frame's is one more than the
previous frame's, Padding and unknown kinds included, so a file's frames form one contiguous run. Values are in
`1..u64::MAX`: 0 and `u64::MAX` are never written. The first frame of a new container has `frame_seq = 1`; a
rewrite (§4.6) numbers the frames of the new file on from one more than the `frame_seq` of the last valid frame of
the old file (committed or not, so acknowledged WAL frames after `log_end` keep their numbers unique), and writes
both of its slots as commits (`seq ≥ 1`), so a database never gives one `frame_seq` to two different frames. Frame
decoding does not interpret the field; the container reader checks it (below).

Validation (ADR-0009), in this order. A frame at offset `o` in input that ends at `E` is *invalid* when:
1. fewer than 32 bytes (the smallest frame) remain, or `frame_magic` differs;
2. its total length overflows `u64` or runs past `E`;
3. `crc32c` does not match;
4. padding or `reserved` bytes are non-zero.

A frame that passes these checks but sets a `flags` bit this reader does not know is *unsupported*, not invalid
(bit0 is the only one assigned). New flags, like any feature older readers cannot handle, also set an
`incompat_flags` bit (SPEC §15), so a reader that opened the container never sees one from a correct writer.

**Streaming and ranged access.** A writer that streams a payload too large to buffer declares `payload_len` in the
header before the payload follows, and stores it uncompressed (a compressed length is unknown until compression
ends); the frame must then hold exactly that many payload bytes. A range reader fetches the 24-byte header first and
sizes the read of the whole frame from its magic and `payload_len`. Before fetching or allocating, it MUST bound the
total length by the end `E` of the container or object (validation rule 2 above); nothing else in the header is
trusted until the whole frame passes the validation checks above (ADR-0009).

**Kinds.** Unknown `kind` values (including 0) with a valid frame are skipped (forward compatibility); a reader
MUST NOT interpret them. Padding frames carry no meaning: their payload exists to align what follows.

**Compressed payload** (flags bit0). The stored payload is `uncompressed_len u64` followed by exactly one zstd
frame with the same rules as a `Zstd` block (SPEC §5.2): it declares a content size equal to `uncompressed_len`,
with no skippable frames and no trailing bytes. `payload_len` counts both. A reader passes the largest payload it
accepts for the frame's kind (decompression-bomb guard): a compressed payload whose `uncompressed_len` is larger is
rejected before decoding, and an uncompressed one longer than it likewise, so the limit means the same whichever
way the writer stored the frame. Either is corruption, as is a malformed compressed payload in a valid frame; both
are found when the payload is decoded, not while scanning.

**Scanning.** A scan reads frames from an offset until the first invalid or unsupported frame, or the end of the
input. It ends in one of four ways, at the offset `o` where it stopped:
- *clean end*: the input ends exactly at a frame boundary;
- *torn tail*: the frame at `o` is invalid and is the last thing written: fewer than 32 bytes remain, or its
  total length runs past `E`, or no non-zero byte follows its end (a frame whose magic differs has no known end,
  so every byte from `o` must be zero). This is the usual result of a crash during an append: a short file, or a
  file whose length grew before all of its data reached the disk (missing data reads as zeros);
- *corruption*: the frame at `o` is invalid and non-zero bytes follow it. Either written data after `o` is lost
  (bit rot, a damaged synced frame), or the crash interrupted an append whose pages reached the disk out of order
  (e.g. delayed allocation leaving a hole inside a large frame). The bytes alone cannot tell these apart;
- *unsupported*: the frame at `o` is unsupported. Nothing is truncated; the container cannot be opened by this
  build.

Recovery: starting at `log_end` from the chosen superblock, scan forward, expecting `frame_seq = next_frame_seq`
at `log_end` and one more for each later frame; a valid frame with any other `frame_seq` (a leftover or misplaced
frame) ends the scan as *corruption* at that frame (ADR-0013). A torn tail or corruption terminates the scan, and
the file is logically truncated at `o`. On a torn tail, before appending again the writer truncates
the file at `o` and syncs it, so frames that once followed `o` cannot reappear behind new ones (ROADMAP 2.4.4).
On corruption the bytes after `o` may hold acknowledged writes (WAL frames synced after `log_end`, SPEC §8.2), so
they are not destroyed silently (ADR-0014): a read-write open searches them for valid frames (at 8-aligned offsets
holding the magic). If one has a `frame_seq` at or above the one expected at `o`, or flags this build does not know
(a newer writer's frame), it may be acknowledged: the open fails, naming it, unless the caller asks for quarantine.
Zeros and stale frames (lower `frame_seq`, ADR-0013) are truncated like a torn tail. Anything else is first copied
to `<name>.quarantine.<o as 16 hex digits>.<nonce>.<n>` next to the container (file and directory synced), then
truncated. Without a path there is nowhere to copy to, and the open fails. The search reads at most four times the
tail's length; past that budget (many bogus frame candidates) nothing is known and the open fails as in the first
case.

### 4.5 Commit protocol (local)
1. Append Segment / WalChunk frames. `fdatasync`.
2. Append Manifest frame referencing live segments. `fdatasync`.
3. Write the *inactive* superblock slot with `seq+1`. `fdatasync`.
4. Publish in-memory state to readers.

A crash before step 3 completes leaves the previous superblock valid ⇒ the prior commit.

The new slot holds `log_end` = the end of the last appended frame (frames appended after the Manifest are committed
with it), `next_frame_seq` = the next frame's `frame_seq`, and a `wal_start` that is a frame start (or `log_end`)
no lower than the previous one: the WAL only advances (§8.2). Appends between commits are made durable in groups,
with one `fdatasync` for every frame appended since the last (group commit); a synced frame survives a crash before
the next commit, and recovery finds it after `log_end` (§4.4). If any write, truncation or `fdatasync` fails, the
writer stops writing: after a failed `fdatasync` the OS may have dropped the unwritten pages, so repeating it proves
nothing. The container is reopened, and recovery decides what survived.

### 4.5.1 Opening a container
- **Create** never truncates or replaces a file (§4.7): the header and both slots (the new-container state of
  §4.3, with a random `db_uuid`) are written to a temporary file `<name>.tmp.<nonce>.<n>` in the same directory and
  synced; the file is hard-linked to the container name, which fails if the name exists; then the temporary name is
  removed and the directory synced. A crash leaves no container or a complete one, and possibly a stale temporary
  file. The file system must support hard links. A writer removes the stale temporary files of creates and compactions
  of its container (`<name>.tmp.*`, `<name>.compact.*`) on request (`sweep_stale_files`, ROADMAP 2.4.15): holding the
  writer lock, no compaction can be in flight, and a temporary file whose own lock is held belongs to a live create,
  which is skipped. Quarantine files (§4.4) are never removed by that sweep: they may hold acknowledged writes, and a
  writer removes them only on an operator's explicit request, naming them (`remove_quarantine_files`; the CLI names
  those listed before its own open). Both kinds can be listed
  by path without opening the container (`side_files`), each temporary file with whether its lock is held
  (ADR-0019, ROADMAP 2.4.19; `persia container sweep`).
- **Writer lock.** A read-write open holds an exclusive lock while it is open, and fails at once if another handle
  holds it, in this process or another (§8.4). On Unix it is an advisory `flock` on the container file; after
  locking, the open checks that the path still names the locked file and reopens if not: a compaction (§4.6) may
  have renamed a new file over it, and the old one's lock is released when its writer moves on. On Windows,
  `LockFileEx` locks are mandatory, so a lock on the container would keep every reader out: the lock is on a file
  `<name>.lock` next to the container instead, created if missing and never removed (removing it would race with a
  writer taking it); since it is never renamed, no recheck is needed (ADR-0016). Read-only opens take no lock. Limit: on NFS,
  Linux emulates `flock` with per-process POSIX locks, so two handles of one process are not excluded from each
  other.
- **Read-only open.** Never writes, truncates or locks. A torn tail or corruption after `log_end` is truncated only
  logically. A container with unknown `rocompat_flags` (§4.2) opens only read-only: a read-write open fails, naming
  the flags. As it holds no lock, a writer may commit or truncate a torn tail while it opens: it checks the active
  slot against a file length taken *after* reading the slots (a commit writes its frames before its slot, §4.5), and
  when its scan from `log_end` hits the end of the file (a writer's recovery truncates a torn tail, never below
  `log_end`, §4.7, and may append again at once, even back to the same length) it scans again up to the file's
  current end, at most 8 times. A short read that persists past those, or a file that shrank below `log_end`, still
  fails the open.
- **Recovery** (§8.6), in this order, for both modes: validate the header (§4.2); pick the active slot (§4.3) and
  check it against the file length (§4.3); check that `manifest_off` holds a valid Manifest frame of `manifest_len`
  bytes; validate the frames of `wal_start..log_end` (§8.6); scan from `log_end` (§4.4). Then by the scan's end:
  clean, the next frame goes at the end of the file; torn tail, a read-write open truncates the file there;
  corruption, a read-write open truncates or quarantines as in §4.4 (or fails) and a read-only open reads up to it;
  unsupported, the open fails. A read-write open then syncs the file before anything else: what it read may still be only in the page
  cache of a writer that died without syncing, and a commit must never reach the disk before the frames it covers
  (§4.5). Valid frames after `log_end` are then durable but not committed (synced WAL frames, §8.2, or the frames
  of a commit that did not complete); the next commit's `log_end` covers them. Recovery reads frames in bounded chunks
  (64 KiB), never a whole frame or tail at once, so its memory does not grow with the size of a frame or of a torn tail
  (only its list of frames from `wal_start` grows with their number; ROADMAP 2.4.12).

### 4.5.2 Platform durability notes
"Sync" in this document means the following calls (Rust's standard library, checked against the pinned toolchain's
sources; ROADMAP 2.4.8, ADR-0016). Each must reach stable storage, including the drive's volatile write cache, before the
call returns:

| Platform | File data (`sync_data`) | File data and metadata (`sync_all`) | Directory entries |
|----------|-------------------------|-------------------------------------|-------------------|
| Linux, other Unix | `fdatasync` (`fsync` where unavailable) | `fsync` | `open` the directory, `fsync` |
| macOS, iOS | `fcntl(F_FULLFSYNC)` | `fcntl(F_FULLFSYNC)` | `open` the directory, `fcntl(F_FULLFSYNC)` |
| Windows | `FlushFileBuffers` | `FlushFileBuffers` | open with `FILE_FLAG_BACKUP_SEMANTICS` and write access, `FlushFileBuffers` |

- macOS `fsync` only hands data to the drive, which may keep it in its cache across a power loss; `F_FULLFSYNC` also
  flushes the drive cache. It is much slower (milliseconds), which is why commits group their syncs (§4.5).
- Linux `fdatasync` flushes the file's data and the metadata needed to read it back (its size), not timestamps;
  ext4, XFS and btrfs also flush the drive cache unless mounted with barriers off (`nobarrier`), which voids these
  guarantees.
- A new or renamed name is durable only after its directory is synced (on Linux a file's `fsync` does not cover its
  entry). Windows directory entries are flushed the same way; NTFS also journals metadata, but only lazily. A volume
  whose directories cannot be flushed (FAT, some network redirectors on Windows) fails every operation that needs
  it with an I/O error, rather than acknowledging a write it cannot make durable.
- A failed sync leaves what reached the disk unknown, and repeating it proves nothing (§4.5): writers stop and
  recovery decides. Network file systems (NFS, SMB) may report success before the server has the data; they are
  outside these guarantees.
- Tests (CI on Linux, macOS and Windows) check that each call succeeds on files and directories; whether a drive
  honors a flush cannot be observed from software, so crash behavior is tested on simulated files (ROADMAP 2.4.11).

### 4.6 Compaction of the container
Dead frames (replaced segments, superseded tombstone sidecars, old manifests, consumed WAL chunks) are reclaimed by
**rewriting the live frames into a new file** and atomically `rename`-ing it over the old one. The container does
not interpret payloads, so the writer (the engine, from its manifest) names the live frames below `wal_start`;
every frame from `wal_start` on is kept, since the WAL range and the frames after `log_end` (acknowledged WAL
frames, §8.2) are needed by recovery.

**Space accounting.** The *live* bytes are the 12288-byte prefix, the live frames below `wal_start`, and everything
from `wal_start` to the end of the valid frames; the rest of that end is *dead*. Compaction is due when dead bytes
reach the dead-space ratio of the end (default 40%). Dead frames in `wal_start..log_end` (an old Manifest) count as
live; they are few and small.

**The new file**, written by the single writer while it holds the lock:
1. The live frames below `wal_start`, in their old order, then the frames of `wal_start..log_end`, each copied with
   its stored payload unchanged and a new `frame_seq`, numbered on from one more than the `frame_seq` of the last
   valid frame of the old file (§4.4). Each frame is validated as it is copied (a corrupt one fails the compaction,
   leaving the old container) and streamed in bounded chunks (64 KiB), like recovery reads it (§4.5.1), so memory
   does not grow with the size of a frame, compressed or not (ROADMAP 2.4.17).
2. A new Manifest frame, which the writer builds from where the frames moved (the offsets it references change).
3. Both slots hold the same commit: `seq` one above the old one, `log_end` the end of the new Manifest, `wal_start`
   the new offset of the old `wal_start` frame (the end of the Manifest if the old WAL range was empty), the old
   `fence_token`, and `next_frame_seq` one above the Manifest's.
4. The old frames after `log_end`, copied the same way after the new `log_end`, so they stay uncommitted.
5. The header is the old one with `format_minor = max(old, own)` (§4.2).

The new file is written under a temporary name `<name>.compact.<nonce>.<n>` in the same directory and locked,
synced, renamed over the container, and the directory synced. The old file is never written, so a crash before
the rename leaves the old container (and a stale temporary file); the rename replaces the name atomically, so the
path names a complete old or new container at every instant. Readers that opened the old file keep reading it
(§4.7). If the directory sync fails after the rename, the writer continues on the new file but stops writing until
reopened, since the rename may not be durable; it still learns where the frames moved. The rename goes to the
canonical path resolved at open, so a symbolic link to the container keeps pointing at it; other hard links to the
file are not supported (they keep the old contents). The writer refuses to rename if that path no longer names its
file: same device and inode on Unix, same volume serial number and file id on Windows (ADR-0021).
Because a compaction changes which file the path names, a read-write open checks after taking the lock that
the path still names the file it locked, and opens again if not (§4.5.1).

### 4.7 Memory-mapped reads
A local reader may memory-map byte ranges of the container instead of reading them (ADR-0010). Only bytes that are
never written again may be mapped: frames in `12288..log_end` of a superblock the reader has read. The header and
superblock pages are rewritten in place, so they are always read with ordinary reads. To keep every mapping valid,
a writer never truncates, hole-punches or rewrites the container below the `log_end` of the superblock it opened
with or has since committed:
- recovery (§8.6) truncates only at the end of the §4.4 scan, which starts at `log_end`; an invalid frame below
  `log_end` is corruption and is never truncated;
- compaction (§4.6) writes a new file instead of shrinking the old one, so existing mappings keep the old file's
  contents; a live mapping does not block the rename over the container on Unix, nor on Windows where the standard
  library renames with POSIX semantics (Windows 10 1607 and later, NTFS; ADR-0016). Elsewhere (FAT, exFAT)
  `MoveFileExW` may refuse to replace a mapped file, and compaction then fails without harm;
- creating a container never truncates an existing file (create-new, or write elsewhere and rename).

A slot damaged after another process read it is outside this contract: recovery may then pick the older slot and
truncate below the `log_end` that process mapped. A fault on a mapped page (a read error, or a truncation by another
program) raises `SIGBUS` on Unix (`EXCEPTION_IN_PAGE_ERROR` on Windows) and ends the process; a reader that cannot
accept that reads with ordinary reads instead.

Mapping is opt-in; ordinary reads are the default (ADR-0017). The automatic choice maps only a container on a local
disk file system (`enable_mapped_reads_if_local`): on Linux, those in an allow-list of `f_type`s (ext2-4, XFS, Btrfs
and other allow-listed local types; ADR-0021), on macOS local volumes other than FUSE, FAT and exFAT, on Windows fixed
and RAM disks. Network, FUSE, overlay, removable and unlisted file systems keep ordinary reads, since a failed page read
there can end the process (ADR-0021, ROADMAP 2.3.10). A container with mapped reads holds one mapping of
`12288..log_end`, made again after each commit and on the new file after a compaction. If making it again fails, the
previous mapping stays (after a compaction: none), so the mapping covers `12288..` the `log_end` of the last mapping
that succeeded. A frame the mapping covers is read from it; any other (after that end) is read with ordinary reads.
Either way the frame is validated on every read (§4.4). On Windows a live mapping keeps the file from being shrunk,
so a read-write open that must truncate a torn or corrupt tail (§4.4, §8.6) fails with an I/O error and leaves the
container unchanged until the mapping is gone; a corrupt tail's quarantine copy (§4.4) may be left behind by each
such attempt.

---

## 5. Segment format

A segment is a **self-contained byte string** with its own footer, usable as a frame payload (local)
or as an independent object (remote). Segments belong to exactly one collection and never change.

### 5.1 Layout
```
[ section 0 ][ section 1 ] ... [ section N ][ footer ][ footer_len u32 ][ magic "PSEG" ]
```
Offsets in a segment are relative to its first byte (a segment is a frame payload or an object). Every section and the
footer start at a multiple of 8, after zero padding (§4.1); sections are block-framed or raw (§5.3). The last 8 bytes,
the **locator**, hold `footer_len` (the footer's length, its CRC included, at most 1 MiB) and the magic `PSEG`. A reader
fetches the segment's last 4096 bytes, which hold the whole footer of a typical segment, and the rest of the footer only
when `footer_len` says it is longer (a segment with many fields; 1024 fields of statistics alone take 16 KiB). The
footer (ADR-0027):
```
0    2   format_major         u16 = 1    (§15; another major is unsupported)
2    2   format_minor         u16 = 0    (readers never branch on it, only on flags)
4    4   reserved             MUST be zero
8    8   incompat_flags       u64        none defined: a set bit is unsupported
16   8   rocompat_flags       u64        none defined: a reader may read a segment with unknown bits, a merge must not rewrite it
24   16  segment_id           u128
40   16  schema_hash          u128       §6.2.5
56   1   schema_encoding_version u8      §6.2.5
57   3   reserved             MUST be zero
60   4   schema_field_count   u32        fields the hash covers: the schema's first field_count fields (§6.2.5)
64   4   doc_count            u32        1..=2^31 (§5.4)
68   4   merge_generation     u32        0 for a flush; one more than the highest input for a merge (§5.5)
72   8   created_at           i64        microseconds since the Unix epoch, UTC
80   4   field_stats_count    u32
84   4   section_count        u32
88   16·field_stats_count     field statistics: field_id u32, docs_with_field u32, sum_field_len u64
..   24·section_count         section table: section_kind u16, reserved u16 (zero), field_id u32, offset u64, len u64
..   2+a  min_id              u16 length, then the smallest document id in the segment (byte order)
..   2+b  max_id              u16 length, then the largest
..   4    crc32c              u32 over every footer byte before it
```
There is no deleted count: deletions live outside the segment (SPEC §8.3). Field statistics list every `text` and
`keyword` field below `schema_field_count`, in ascending field id, including those no document has (zeros):
`docs_with_field` counts the documents with at least one token (`text`) or value (`keyword`) in the field, and
`sum_field_len` sums the field lengths of §5.3.4. The section table lists every section in segment order, which is
ascending `(field_id, section_kind)`: a field's sections in kind order, field by field, then the segment-level sections
(`field_id` `0xFFFF_FFFF`, §5.3). `min_id`/`max_id` allow routing and pruning by document id. A reader checks the
locator, bounds `footer_len`, and verifies the CRC before it uses any other footer field. Then each of these is
corruption:
- `footer_len` below 98 (the 88-byte fixed part, two ids of at least 1 byte with their `u16` lengths, and the
  CRC) or above 1 MiB, or a footer start (`len − 8 − footer_len`) that is negative or not a multiple of 8; bytes
  between `max_id` and the CRC (the CRC is the footer's last 4 bytes, so `max_id` must end exactly there);
- a non-zero reserved byte; `doc_count` outside `1..=2^31`; `schema_field_count` above the current schema's field
  count, or a schema hash other than its hash over that many fields (§6.2.5);
- `field_stats_count` other than the number of `text` and `keyword` fields below `schema_field_count`, or statistics
  out of field order, of another field type, or with `docs_with_field > doc_count`;
- a section table not in strictly ascending `(field_id, section_kind)`; a section whose offset is not a multiple of
  8, that starts before the previous one ends or 8 or more bytes after it, or that ends after the footer start (or
  more than 7 bytes before it); non-zero padding;
- `min_id` or `max_id` outside 1..=512 bytes, or `min_id > max_id`.

### 5.2 Blocks
All large sections are split into **fixed-size logical blocks (default 16 KiB uncompressed)**, each
with a header holding its CRC32C, codec and lengths (layout below). Blocks are independently decodable ⇒ range requests
and per-block caching work on object storage.

**Codecs** (ADR-0005). Every block names its codec in the `codec` byte of its header:

| `codec` | Name | Payload |
|---|---|---|
| 0 | `None` | the bytes as is; length must equal `uncompressed_len` |
| 1 | `Zstd` | exactly **one** zstd frame (RFC 8878, magic `0xFD2FB528`) whose header declares a content size equal to `uncompressed_len`; no skippable frames, no trailing bytes. Writers set neither a dictionary id nor the frame checksum (the block CRC covers the bytes); readers accept a checksum, and a frame that needs a dictionary fails to decode |

Any other value is corruption. Readers apply a **decompression-bomb guard**: the caller passes the largest
`uncompressed_len` it accepts (for blocks, the section's block size), and a larger value is rejected before any
allocation or decoding. A payload that expands, or declares that it expands, to another length is corruption.
Writers choose the codec per block (e.g. `None` when zstd does not shrink it); the zstd level is not recorded.
Frame payloads compress with the same zstd rules (SPEC §4.4). Which `section_kind`s are block-framed is in §5.3.

**Block layout** (ADR-0006). A block is a 16-byte header followed by its payload; blocks are byte-aligned
and not padded:
```
0   4  crc32c            u32  over bytes 4 up to the block's end (from the block table): the rest of the
                              header and the payload
4   1  codec             u8   (table above)
5   3  reserved          MUST be zero
8   4  uncompressed_len  u32
12  4  stored_len        u32  (payload length)
16  ..  payload           stored_len bytes
```
The CRC is verified before any other field is interpreted; its range comes from the block table, not from
`stored_len`. In this section "block" means such a framed block, not the 128-docid postings blocks or 1024-value
FOR blocks of §5.2.1, which are encoded inside a framed block's stream.

**Block-framed sections** (ADR-0006). A block-framed section stores a logical byte stream of `total_len`
bytes cut into blocks of `block_size` uncompressed bytes: block `i` holds stream bytes
`[i·block_size, min((i+1)·block_size, total_len))`, so every block but the last is full, the last holds
`1..=block_size` bytes, and an empty stream has no blocks. Stream position `p` is in block `p div block_size`.
The blocks are followed by the **block table**, which ends the section:
```
[ block 0 ][ block 1 ] ... [ block n-1 ][ block table ]

block table (8·n + 24 bytes)
0      8·n  block_end    u64 each: end of block i, relative to the section start
                         (block 0 starts at 0, block i at block_end[i-1])
8n     8    total_len    u64
8n+8   4    block_size   u32  (1..=1 MiB, i.e. at most 1048576)
8n+12  4    block_count  u32  (n)
8n+16  4    reserved     u32  MUST be zero
8n+20  4    crc32c       u32  over table bytes 0..8n+20
```
A reader reads `block_count` from the fixed 24-byte trailer, checks that the table fits in the section, then
verifies the table CRC before using any other field. The following are corruption: a `block_size` outside
`1..=1 MiB`; `block_count ≠ ceil(total_len / block_size)`; a block shorter than its 16-byte header; a block
longer than its header plus `block_size`; a last `block_end` (or `0` when `n = 0`) other than the table's start.
The length bound means fetching one block never costs more than `16 + block_size` bytes, whatever its header
says; writers store a block as `None` when zstd would not shrink it, which keeps every block within it. When
block `i` is decoded, its header must agree with the table: `stored_len = block_end[i] - start - 16`, and
`uncompressed_len` equals the length the formula above gives for block `i`. A block's errors name its index.

The table's `block_size` is the decompression-bomb limit for its blocks, so a reader needs nothing outside
the section to decode it (no footer field). Any reader may use a smaller limit of its own.
Records in a block-framed section are addressed by stream position, not by block, and every offset that points
into a block-framed section (from TermInfo, Skip, ...) is a stream position. So a record may span blocks: a
reader decodes every block that overlaps the record. In particular a DocStore row may be larger than one
block (rows are addressed through a record array, §5.3.6). Whether a section is block-framed is
fixed by its `section_kind` (SPEC §5.3), and each block names its own codec, so the section
table has no `codec` field.

### 5.2.1 Integer codecs
Building blocks of the Postings, Freqs, Skip and Columns sections (SPEC §5.3), ADR-0004. They live inside
blocks, so they are byte-aligned (the 8-byte alignment of SPEC §4.1 applies to frames and sections). Every
width, count and length below is validated before use; anything out of range is corruption.

**Delta coding (sorted `u32`).** A strictly increasing sequence `v0 < v1 < ...` is stored as gaps
`g_i = v_i - v_(i-1) - 1`, where `v_(-1)` is the previous block's last value, or `-1` for the first block of
a list (so the first gap is `v0` itself, and consecutive ids cost 0 bits). A decoded value above `u32::MAX`
is corruption.

**Bit-packed block (128 × `u32`, width `w` in `0..=32`).** Exactly `16·w` bytes, read as `4·w` little-endian
`u32` words; word `k` is lane `k mod 4`, row `k div 4`. Value `i` belongs to lane `i mod 4`, slot `i div 4`.
Each lane is a little-endian bit stream made of its `w` words (row 0 first, least significant bit first), and
slot `j` holds bits `[j·w, (j+1)·w)`. (This is the SIMD-BP128 "vertical" layout: one row is one 128-bit SIMD
word, so a vectorised decoder reads the same bytes as the scalar one.) Writers use the minimal width, the
bit length of the largest value; a width above 32 is corruption.

**Postings blocks.** A posting list of `n` docids is `n div 128` full blocks, then a tail block if
`n mod 128 > 0`. Docids are delta coded across the whole list.
```
full block                                  tail block
0  4  last_docid u32 (absolute)             0  1  count  u8 (1..=127)
4  1  width      u8  (0..=32)               1  .. count gaps, varint u32 each
5  16·width  bit-packed gaps
```
`last_docid` lets a reader skip a full block without unpacking it. It must be at least `prev + 128` (127 for
a list's first block), checked when the header is read, whether the block is skipped or decoded. It must also
equal the block's last decoded docid. A violation of either is corruption. So a list stays strictly increasing
however skipping and decoding are mixed. A tail's `count` must equal the term's `df mod 128`. Docids must be
below the segment's `doc_count` (SPEC §5.4). The section reader checks both; the block codec cannot.
Block-max data (`max_tf`, `min_norm`) is not part of these blocks: it lives only in the Skip section. Term
frequencies live in the Freqs section, in blocks aligned with these (§5.3.2).

**Frame-of-reference (FOR) block (numeric columns).** A column is cut into FOR blocks of exactly 1024
values; only the last may be shorter. So docid `d` is value `d mod 1024` of block `d div 1024`. Each block
stores its values as offsets from their minimum, horizontally bit-packed so any value can be read in O(1):
```
0   8  min    u64  (order-preserving key, below)
8   4  count  u32  (1..=1024)
12  1  width  u8   (0..=64)
13  ceil(count·width / 8) bytes
```
Value `i` is `min + (bits [i·w, (i+1)·w) of a little-endian, least-significant-bit-first bit stream)`. The unused
high bits of the last byte are zero. A count outside `1..=1024` or a value above `u64::MAX` is corruption. The column reader checks that every block
but the last has `count = 1024` and that the counts sum to the segment's `doc_count`; a mismatch is corruption.
Writers use the true minimum and the minimal width.

**Order-preserving keys.** Signed and floating-point columns are stored as `u64` keys whose unsigned order is
the value order. `i64`: flip the sign bit. `f64` (the IEEE-754 bits): if the sign bit is clear, set it;
otherwise invert all bits. This matches `f64::total_cmp` (`-0.0 < +0.0`, and NaNs sort at the ends), and every
bit pattern round-trips exactly. Writers never key `-0.0`, NaN or an infinity in an `f64` column, nor a
`datetime` outside its §6.4.1 range: a document holds none of them (§6.4.1). A document's key that
decodes to one is corruption. A reader that decodes one value at a time need not check it; a check of the whole segment
(every column read in full, as `persia inspect segment --verify` does) must report it. A document without a value
holds an unspecified key (§5.3.5), which is not checked.

### 5.3 Sections (per segment)
| Section | `section_kind` | Per | Framed | Content | Encoding |
|---|---|---|---|---|---|
| `TermDict` | 1 | text/keyword field | raw | term → index into TermInfo | FST (§5.3.1) |
| `TermInfo` | 2 | text/keyword field | block | `{postings_off, freqs_off, skip_off, positions_off, ttf, df}` | fixed-width records (§5.3.1) |
| `Postings` | 3 | text/keyword field | block | doc ids | blocks of **128** delta-coded, bit-packed; tail block varint (§5.2.1) |
| `Freqs` | 4 | text/keyword field | block | term frequencies | blocks of 128 bit-packed, aligned with Postings (§5.3.2) |
| `Skip` | 5 | text/keyword field | block | per block: `last_docid`, offsets, `max_tf`, `min_norm` | fixed-width entries; enables skipping and block-max WAND (§5.3.2) |
| `Positions` | 6 | text field (opt-in) | block | token positions | delta + bit-packed; only if field has `positions: true` (§5.3.3) |
| `Norms` | 7 | text field | block | quantized field length per doc (1 byte, Lucene-style) | array (§5.3.4) |
| `Columns` | 8 | keyword/i64/f64/datetime/bool field | block | doc values for filter/sort/facet | keyword = dictionary + ords; numerics = FOR; bool = roaring (§5.3.5) |
| `DocStore` | 9 | segment | block | stored fields of documents (CBOR, §6.4.2) | record array, zstd blocks (§5.3.6) |
| `IdMap` | 10 | segment | raw | external id → internal `docid` | FST (§5.3.6) |
| `IdList` | 11 | segment | block | internal `docid` → external id | record array (§5.3.6) |
| `BlobRefs` | 12 | segment | block | docid → blob_ref | record array (§5.3.6) |

Deleted docids are **not** a segment section: segments are immutable, so deletions are stored in
tombstone sidecars (SPEC §8.3).

**Framing** (ROADMAP 5.2.9, ADR-0027). A *block* section is block-framed (§5.2): offsets into it are stream positions.
A *raw* section is its payload followed by a `u32` CRC32C of the payload: the two FSTs, which are read whole and used
in place. A CRC proves only that the bytes are the ones written, so a reader checks an FST's structure before using
it: header version 3; nodes laid out back to back from the end of the 16-byte header to the root, whose last byte is
the one before the footer; packed sizes of at most 8 bytes; every transition pointing to the last byte of a node below
its own, or to the empty node (so no cycles); transitions in strictly increasing byte order, with an index (when
present) that agrees with them; output sums that fit a `u64` along every path; and a key count equal to the keys the
nodes hold. Anything else is corruption, as is a path longer than the longest key of §5.3.1 and §5.3.6. Any other `section_kind` is corruption (a new kind needs an `incompat_flags` bit), and so is a kind under a
field of the wrong type, two sections with the same `(section_kind, field_id)`, or a `field_id` at or above
`schema_field_count` (other than `0xFFFF_FFFF` for the segment-level kinds 9–12, which use nothing else).

**Presence.** A section that would hold nothing is left out, and readers treat a missing one as empty:
- a `text` field with at least one token in the segment has TermDict, TermInfo, Postings, Freqs and Norms, Positions
  if `positions: true`, and Skip if some term has `df ≥ 128`; with none, it has no sections;
- a `keyword` field with at least one value has TermDict, TermInfo, Postings, Freqs, Columns, and Skip as above;
- an `i64`, `f64`, `datetime` or `bool` field with at least one value has Columns; a `stored` field has none;
- every segment has DocStore, IdMap and IdList, and BlobRefs if a document has a blob.

#### 5.3.1 Term dictionary
`TermDict` is an FST (the `fst` crate's map format, version 3) whose keys are the field's terms (UTF-8 bytes, in byte
order) and whose value is the term's ordinal: its rank in that order. A `keyword` field's terms are its values, so no
path of its FST is longer than the longest keyword value (32 KiB, §6.4.1). A `text` field's terms are analyzer tokens,
bounded by the same 32 KiB: a builder rejects a document whose analyzer yields a longer token (an analyzer need not
apply the length guard of §6.3.1, and the guard's limit is the analyzer's). In any `TermDict`, a path longer than
32 KiB is corruption. `TermInfo` is the stream of one 56-byte record per term, record `i` at position `56·i`:
```
0   8  postings_off   u64  start of the term's list in Postings
8   8  freqs_off      u64  start of its frequencies in Freqs
16  8  skip_off       u64  start of its entries in Skip; 0 when df < 128 (no entries)
24  8  positions_off  u64  start of its positions in Positions; 0 without Positions
32  8  ttf            u64  sum of the term's frequencies
40  4  df             u32  documents with the term, ≥ 1
44  4  max_tf         u32  largest frequency of the term in any document
48  1  min_norm       u8   smallest norm byte (§5.3.4) of the term's documents; 0 for a keyword field
49  7  reserved       MUST be zero
```
`max_tf` and `min_norm` bound the term's score before its list is read (MaxScore and block-max WAND, §7.3), without
scanning its Skip entries; they are the maxima and minima of those entries when it has them.
A `text` field's terms are the token texts of its analyzer (§6.3); a `keyword` field's are its values, as they are.
Lists MUST be written in term order, back to back: the `postings_off`, `freqs_off` and (with Positions)
`positions_off` of record `i + 1` are where term `i`'s data ends in those streams, and the last term's ends at the
stream's end; likewise the `skip_off` of each term with `df ≥ 128` is where the previous such term's entries end. A reader finds a list's length that way; anything
else is corruption, as is a `df` of 0, an offset past its stream, `skip_off` other than 0 when `df < 128`, or
`positions_off` other than 0 without Positions.

#### 5.3.2 Postings, freqs and skip entries
A term's postings are the blocks of §5.2.1. Its frequencies (`tf ≥ 1`, occurrences of the term in the document's
field) are stored as `tf − 1`, in blocks aligned with the postings blocks: a full block is a width byte then 128
values bit-packed as in §5.2.1 (`16·width` bytes); the tail is `df mod 128` varints, without a count. In general, a
**run** of `n` values is `n div 128` such packed groups followed by `n mod 128` varints.

A term with `df ≥ 128` has one 40-byte `Skip` entry per postings block, tail included (`ceil(df / 128)` entries):
```
0   4  last_docid      u32  the block's last docid
4   4  max_tf          u32  largest frequency in the block
8   8  postings_off    u64  the block's start in Postings
16  8  freqs_off       u64  its start in Freqs
24  8  positions_off   u64  its run's start in Positions; 0 without Positions
32  1  min_norm        u8   smallest norm byte of the block's documents (§5.3.4); 0 for a keyword field
33  7  reserved        MUST be zero
```
The first entry's offsets equal the TermInfo's. A shorter list is one tail block and needs none.

#### 5.3.3 Positions
A token's position is its position in its value (§6.3.1) plus the value's base. `base_0 = 0` and
`base_(i+1) = base_i + end_i + 100` (`POSITION_GAP`), where `end_i` is one more than the largest position of a token
of value `i` within that value, or 0 when it has none: so a phrase never matches across values. A position above
`u32::MAX` is not representable; the builder rejects the document. For each postings block of a term, its documents'
positions form one run (§5.3.2) of `Σ tf` values: per document, in docid order, its `tf` positions in non-decreasing order (stacked tokens may share
one), the first as is and each next one as the difference to the previous (0 for a repeat). Each block's run starts at its Skip entry's
`positions_off` (the first at the TermInfo's).

#### 5.3.4 Norms
The **field length** is the number of tokens the analyzer produced for the field, all values together (for a
`keyword` field, which has no Norms, the number of values). `Norms` is `doc_count` bytes, byte `d` the norm of docid
`d`, 0 for a document without tokens. A length `n` is stored as Lucene's `SmallFloat.intToByte4`: `n` itself below
24; otherwise `24 + c` where, for `x = min(n, 2^31 − 1) − 24` of bit length `k`, `c = x` when `k < 4`, else
`c = ((x >> (k − 4)) & 7) | ((k − 3) << 3)`. A byte `b ≥ 24` stands for `24 + v`, `v = c` when `c < 8`, else
`v = ((c & 7) | 8) << ((c >> 3) − 1)` (`c = b − 24`). Encoding rounds down: the decoded length is exact below 40,
never above `n`, and below it by less than `(n − 24) / 8` (under 12.5 %). The code is monotonic, so `min_norm` bounds
a block's shortest field.

#### 5.3.5 Columns
A `Columns` stream starts with a tag byte and 7 zero bytes. A **FOR column** of `count` values is `count u64`, then
the end of each of its `ceil(count / 1024)` FOR blocks (`u64`, relative to the first block), then the blocks (§5.2.1);
every block but the last has 1024 values. A **bitmap** is `len u64` then `len` bytes of a roaring bitmap in the
portable serialization format; a **presence** bitmap of length 0 means every document has a value.
- **Numeric** (tag 1; `i64`, `f64`, `datetime`): presence, then a FOR column of `doc_count` order-preserving keys
  (§5.2.1). A document without a value holds an unspecified key (writers store its block's minimum).
- **Bool** (tag 2): presence, then the bitmap of the documents whose value is `true` (always written, so its `len` is
  never 0; a subset of the present ones).
- **Keyword** (tag 3): `starts_off u64` and `ords_off u64`, where the two FOR columns below start (stream positions,
  so a reader fetches any part without reading what precedes it); the dictionary, `count u64` (at least 1), `count`
  value ends (`u64`, relative to the first value), the values (distinct, strictly increasing bytes; ord `i` is value `i`); then a FOR column of `doc_count + 1` starts and
  a FOR column of ords: document `d` has ords `starts[d]..starts[d+1]`, strictly increasing (its distinct values). An
  empty list holds no ords, so it reads as missing. `starts[0] = 0`, starts never decrease, the last start is the number
  of ords, and every ord is below `count`.

Anything else is corruption: an unknown tag, non-zero reserved bytes, a FOR column whose `count` differs from the
one stated above or whose block ends do not increase, a bitmap that does not deserialize or holds a docid at or above
`doc_count`, trailing bytes after the last part.

#### 5.3.6 Documents, ids and blobs
A **record array** of `n` records is `n` record ends (`u64`, relative to the first record), then the records
back to back: record `d` spans `[end[d−1], end[d])` (`end[−1] = 0`). `DocStore` holds the documents' rows (§6.4.2),
`IdList` their ids (UTF-8 bytes) and `BlobRefs` their `BlobRef` encodings (§9.1), empty for a document without a blob;
each has `doc_count` records, in docid order. A record may span blocks (§5.2). Ends that decrease, or a last end
other than the stream length minus `8·n`, are corruption, and so is an empty `IdList` record or a non-empty `BlobRefs`
record that is not exactly one `BlobRef` encoding. `IdMap` is an FST from each id to its
docid; no path of it is longer than the longest id (512 bytes, §6.4.1), and a segment holds an id at most once (the builder rejects a duplicate; the memtable keeps one version per id, and a flush tombstones the older segments' copies, §8.3).

### 5.4 Internal doc ids
`docid` is a segment-local `u32`, dense `0..doc_count`. Segments MUST NOT exceed `2^31` docs.
Doc order within a segment is insertion order unless the collection declares `index_sort` (optional, post-v1).

### 5.5 Merging
Merging reads N segments, drops tombstoned docs, remaps docids, and writes one new segment
(fresh statistics). Merge policy: tiered (see 8.5).
The inputs' live documents keep their order, input by input in the plan's order (§8.5). Nothing is analyzed
again: postings, frequencies and positions are copied with their docids remapped, and column values, `DocStore` rows
(as stored, ADR-0023), ids and blob references are copied as they are. Every statistic (term statistics, norms, field
statistics) is computed from what was copied, so the merged segment is byte for byte the segment a flush of the
same documents in the same order writes, with the same `segment_id`, `created_at` and `merge_generation`, whenever a
flush accepts them; rows that a flush would reject (extras of a collection that became `strict`, §6.4.2) are copied as
stored all the same. Each input's terms are added to one dictionary that the writer sorts, as for a flush. The output
uses the collection's current schema; an input written before fields were added (§6.2.4) has no values for them, and
every input must be read with that schema. A merge refuses an input with unknown `rocompat_flags` (§5.1) and an id
live in two inputs (a commit leaves at most one, §8.3). A merge reads its inputs as any reader does, without
`verify`: damage only `verify` finds is copied (and found by verifying the merged segment, which happens before a
commit adds it, ROADMAP 6.5.3), except an `IdList`, `IdMap` or `BlobRefs` that disagrees with the rows, which the
rows override (ADR-0031, ROADMAP 6.5.2).

---

## 6. Schema, collections, documents

### 6.1 Field types
| Type | Indexed as | Notes |
|---|---|---|
| `text` | analyzed terms + norms (+positions opt-in) | BM25F candidate |
| `keyword` | exact term, doc values | filters, facets, sort |
| `i64`, `f64` | doc values (+ range via column scan/skip) | |
| `bool` | doc values | |
| `datetime` | `i64` microseconds UTC | |
| `stored` | DocStore only | not searchable |

Every document additionally has: `id` (string, ≤ 512 bytes, unique per collection) and optional `blob` (SPEC §9).

### 6.2 Collection definition
```jsonc
{
  "name": "products",
  "fields": {
    "title": { "type": "text", "analyzer": "es", "boost": 3.0, "positions": true, "k1": 1.2, "b": 0.75 },
    "body":  { "type": "text", "analyzer": "es", "boost": 1.0 },
    "brand": { "type": "keyword", "facet": true },
    "price": { "type": "f64" },
    "sales": { "type": "i64" }
  },
  "ranking": { "static_rank": { "field": "sales", "weight": 0.1, "fn": "log1p" } },
  "relaxation": { "enabled": true, "min_results": 1, "levels": "default", "fallback": "static_rank" },
  "synonyms": [["tv","television"]]
}
```
- Schema is **declared at creation**. Adding fields is allowed (existing docs read as missing); changing a field type is NOT
  (§6.2.4).
- Schema hash is recorded in every segment; the manifest holds the current schema (§6.2.5).

#### 6.2.1 Keys, names and limits
The definition is parsed **strictly**: an unknown key at any level, a duplicate key, or an option that does not apply to
the field's type is an error, never ignored (ADR-0022). Every level is a JSON object: the positional array form serde would
otherwise accept is rejected. A definition is at most 4 MiB of JSON.

| Key | Required | Default | Rule |
|---|---|---|---|
| `name` | yes | | 1–64 bytes, `[a-z][a-z0-9_-]*` (it appears in storage keys, §10.1) |
| `fields` | yes | | object, at most **1024** fields |
| `ranking` | no | no static rank | §6.2.3 |
| `relaxation` | no | all defaults | §6.2.3 |
| `synonyms` | no | `[]` | at most 10 000 groups; a group has 2–64 distinct terms; a term is 1–256 bytes, not only Unicode `White_Space`, without control characters; at most 1 MiB of terms in total. Stored as given; analyzed by the L1 level (§7.6) |
| `strict` | no | `true` | unknown document fields rejected (`true`) or kept as `stored` (`false`), §6.4 |

Field names are 1–64 bytes, `[A-Za-z][A-Za-z0-9_]*`. Names starting with `_` are reserved for the engine, and so are `id`
and `blob` in any letter case (every document has them, §6.1).

Every field has a **field id** (`u32`), the number segments use for it. A definition gives either no `id` (the engine
assigns `0..n` in ascending byte order of the names, so the order of keys in the JSON never matters) or an `id` on every
field, which must then be exactly `0..n`. The engine writes the ids when it serializes a schema.

#### 6.2.2 Field options
| Type | Options (default; valid range) |
|---|---|
| `text` | `analyzer` (`"standard"`), `boost` (`1.0`; `0 < boost ≤ 1000`), `positions` (`false`), `k1` (`1.2`; `0 ≤ k1 ≤ 10`), `b` (`0.75`; `0 ≤ b ≤ 1`), `stored` (`true`) |
| `keyword` | `facet` (`false`), `stored` (`true`) |
| `i64`, `f64`, `bool`, `datetime` | `stored` (`true`) |
| `stored` | none (always stored, never indexed) |

`stored: false` keeps the value out of the `DocStore` (it is indexed but `get` does not return it). Floats must be finite;
`-0.0` reads as `0.0`. `analyzer` names a built-in analyzer (§6.3) as `"name"` (its current version) or `"name@version"`;
the engine writes `"name@version"`, so a stored schema pins the version it was indexed with. A definition naming an
analyzer or version this build does not have is rejected (unknown analyzer). A stored schema (§8.7) naming a
well-formed one it lacks (a built-in name of the shape of §6.3.3 and a canonical version, such as `es@2` or `nl@1`
read by a build that only has `es@1`) was written by a newer build: like an unknown format feature (§15), it is
reported as *unsupported*, never as corruption; a malformed one (`es@01`, `ES@1`) is corruption. As with an unknown format
feature, the whole manifest is then unsupported: a build cannot open a database one of whose collections it cannot
analyze, since it could neither query that collection nor keep it intact on its next commit.

#### 6.2.3 Ranking and relaxation
`ranking.static_rank` (optional): `field` (required: an `i64`, `f64` or `datetime` field), `weight` (`1.0`;
`0 ≤ weight ≤ 1000`), `fn` (`"log1p"` or `"linear"`, default `"log1p"`). The scorer adds `weight · fn(v)` (§7.2), where `v` is the field's value as
a float (`datetime`: microseconds since the epoch, as stored). Both functions see only `max(v, 0)`: `log1p(v)` is
`ln(1 + max(v, 0))` and `linear(v)` is `max(v, 0)`. A document without the field, or whose value is not finite,
contributes 0, and a product above `f64::MAX` saturates at `f64::MAX`, so the contribution is always finite and never
negative.

`relaxation` holds the collection defaults of the cascade (§7.6); a query's `relax` overrides them:

| Key | Default | Rule |
|---|---|---|
| `enabled` | `true` | `false` runs L0 only |
| `min_results` | `1` | `1–10 000` |
| `levels` | `"default"` | `"default"` (every level after L0, as the engine defines them) or an array of `"synonyms"`, `"drop_terms"`, `"prefix"`, `"fuzzy"`: distinct, in cascade order. L0 always runs and is not listed; L5 is `fallback`. `[]` runs L0, then the fallback |
| `fallback` | `"static_rank"` | `"static_rank"` or `"none"`; without `ranking.static_rank`, `static_rank` returns the most recently indexed documents |
| `fill` | `false` | §7.6 |
| `filters` | `"never"` | `"never"` or `"last_resort"` |
| `budget_ms` | `50` | `1–60 000` |

#### 6.2.4 Evolution
A new definition replaces the current schema when, matching fields **by name**:
- the collection `name` is the same;
- every current field is still present (fields are never removed in v1) with the same `type`, `analyzer` (name and
  version), `positions`, `facet` and `stored`: these shape what is indexed, so changing them needs a reindex into a new
  collection (§6.3);
- added fields are allowed: they get the next ids in ascending byte order of their names, and documents indexed earlier
  read as missing them. Ids in the new definition are ignored (they are internal): existing fields keep theirs.
- an `analyzer` given without a version binds to the field's current version when it names the same analyzer, so
  re-applying an unchanged definition after a release adds a newer analyzer version is not a change (the field keeps
  its version); a pinned different version (`"es@2"` for a field at `es@1`) or another analyzer still is.

`boost`, `k1`, `b`, `ranking`, `relaxation`, `synonyms` and `strict` may change freely: they act at query or write time.

#### 6.2.5 Canonical encoding and schema hash
The **schema hash** identifies what a segment was indexed with: XXH3-128 (§4.1's fixed variant) of the canonical
encoding of the collection's *index definition*: its name and, per field, everything §6.2.4 forbids changing. Query-time
settings (`boost`, `k1`, `b`, `ranking`, `relaxation`, `synonyms`, `strict`) are not covered, so tuning them keeps the hash.
```
u8      encoding_version = 1
str     collection name                  (str = varint u32 byte length + UTF-8 bytes)
varint  field_count
per field, in ascending field id (ids are 0..n, so the id is the position):
  str     name
  u8      type        1 text · 2 keyword · 3 i64 · 4 f64 · 5 bool · 6 datetime · 7 stored
  varint  option_count
  per option, in ascending tag:  u8 tag, value
          1 analyzer   str name, varint u32 version    (text)
          2 positions  u8 0|1                          (text)
          3 facet      u8 0|1                          (keyword)
          4 stored     u8 0|1                          (every type but stored)
```
Varints are §4.1's LEB128. Every encoding-version-1 option of the field's type is written, defaults included. The encoding is frozen: a
later option gets a new tag and is written only when its value differs from the behavior before the option existed, so an
older schema keeps its hash; anything else is a new `encoding_version`. Golden vectors pin the hash (ADR-0022).
A segment records the hash, its `encoding_version` and the number of fields it was indexed with (§5.1). Evolution only
appends fields and never changes what the hash covers (§6.2.4), so the hash of any earlier version is the current
schema's hash over its first `field_count` fields: a reader verifies an old segment against the current schema alone.
Readers keep the encoder of every `encoding_version` they accept.

### 6.3 Analyzers
Pipeline: `char_filter* → tokenizer → token_filter*`. Char filters rewrite the field value before tokenization, and
the analyzer maps token spans back to the original value (§6.3.1); no built-in analyzer has one.
Built-ins (§6.3.3): `standard` (Unicode UAX#29 words, NFKC, lowercase), `keyword`, `whitespace`, and the language
analyzers `en`, `es`, `pt`, `fr`, `de` and `it` (stopwords + Snowball stemmer + accent folding).
Analyzers are looked up by name and version (`persia_analysis::AnalyzerRegistry`). Analyzer name+version is recorded
in the schema (§6.2.2); a released version never changes its chain or tables, so any change to a built-in analyzer
is a new version. Only a change of the Unicode data or algorithms the build uses (a Unicode version bump, or a
dependency or toolchain update that fixes them within one) can change a released version's output, and the analyzer
then gets a new version that supersedes it (§6.3.1). This holds as far as the fingerprints of §6.3.1 see: a change visible only
outside their probe contexts could move an output unversioned. Changing a field's analyzer requires a reindex into a new collection
(§6.2.4).

#### 6.3.1 Tokens and tokenizers
A token is `{text, start, end, position}`. `start..end` is its byte span in the field value, on UTF-8 character
boundaries; a tokenizer sets `text` to that span of its input, and token filters may rewrite `text` but never the
span, so the span always points at the characters the token came from (through char filters, which change the
tokenizer's input, the analyzer maps spans back to the original value, see below). `position` counts tokens from 0 within one value;
a filter that removes a token leaves a gap, so a phrase never matches across it. The tokens a tokenizer emits for one
value have strictly increasing, non-overlapping spans (filters that stack tokens relax this, §6.3.2). Tokenizers never fail and never panic, on any UTF-8 input. Tokenization is
defined by one Unicode version (17.0.0; `persia_analysis::UNICODE_VERSION`), and CI fails if a dependency or toolchain
update moves it unnoticed. Each current built-in version has a fingerprint of its output over every Unicode scalar
value in a set of probe contexts chosen to tell the UAX #29 word-break classes apart (alone, between letters and
digits, after `a.` and `1.`, before a quote, doubled, around a period, next to an accent), and a test, run on every
pull request, lists the analyzers whose fingerprint moved. Any dependency or toolchain change that moves one (moving
to another Unicode version, or a fix within one) gives a new version, with the same chain, to every built-in analyzer
whose fingerprint it moves, and only to those (ADR-0025); a change visible only outside the probe contexts can go
unnoticed. The versions they supersede stay available and run on the build's Unicode tables, as a build has only one set: a field
indexed with one keeps working, and only text affected by the changed tables or algorithms may analyze differently
than when it was indexed, until the collection is reindexed at the new version (§6.2.4).

- **standard**: Unicode word boundaries (UAX #29), keeping the segments that contain an alphanumeric character;
  punctuation, symbols and emoji produce no token (a keycap emoji such as `1️⃣` keeps its digit and is one).
  Consequences of UAX #29, relied on and tested: numbers with inner separators stay whole (`3.14`, `1,000`); an
  apostrophe between letters joins them (`don't`, `O'Neil`, also `’`); emails and URLs split at `@` and `/` while
  dotted names stay whole (`ana@example.com` → `ana`, `example.com`); a colon joins letters on both sides
  (`mailto:ana`, `key:value`) and splits otherwise (`https://`, `12:30`); `_` joins (`snake_case`); scripts written
  without spaces (Han, Hiragana, Thai, ...) fall back to one token per character, while Katakana runs and Hangul
  words stay whole. No dictionary segmentation in v1.
- **whitespace**: splits at Unicode `White_Space`; everything else, punctuation included, stays in the token.
- **keyword**: the whole value is one token (none for an empty value).

**Char filters.** A char filter consumes the field value from its start in pieces of whole characters, keeping each
piece or replacing it with other text (possibly empty, which deletes it; a replaced empty piece is an insertion), and
never fails or panics. The analyzer tokenizes the last filter's output and maps every span back through each
filter, last first, before any token filter runs, so spans always index the original value: an offset in kept text
maps to the same character of the input; a span start inside a replacement maps to the start of the replaced piece
and a span end inside one to its end, so the span covers every character its text came from; at a deletion a start
maps past the deleted piece and an end before it (deleting `<b>` from `x<b>bold` gives the token `xbold` the span
`x<b>bold`; replacing `<b>` with a space gives `bold` the span `bold`). Mapped spans lie on character boundaries
of the original value, and their starts and their ends never decrease; they need not be strictly increasing or
disjoint, as one replacement can yield several tokens (`½` → ` one half ` gives `one` and `half` the span of `½`) or
the end of one token and the start of the next, and a token made only of inserted text has an empty span at the
insertion point (when chained filters map such an empty span onto a deletion, it takes the position before the
deletion). Positions are unaffected.

**Max token length.** A length guard bounds token texts in bytes of UTF-8 (default 255): `truncate` (default) keeps
the longest prefix that fits and ends on a character boundary (a token whose first character does not fit is
dropped); `drop` removes the token. Kept tokens keep their span and position. Which built-in analyzers apply the
guard, and with which limit, is part of their definition (§6.3.3). Whatever the guard, a field cannot index a token longer
than 32 KiB: the document is rejected (§5.3.1).

#### 6.3.2 Token filters
A token filter rewrites, removes or adds tokens; it never changes a token's span, and never fails or panics. A filter
that leaves a token's text empty removes the token. Removing a token leaves a gap in positions (§6.3.1). Filters that
add tokens (edge n-grams) stack them on the position and span of the token they came from, so after such a filter
positions are non-decreasing rather than strictly increasing; spans of different positions still increase and do not
overlap. Every filter is defined by the Unicode version of §6.3.1, and the embedded tables and algorithms below (stopword
lists, folding table, stemmers) are part of the definition of every analyzer that uses them: changing one changes
those analyzers' versions.

- **nfkc**: Unicode NFKC normalization of the token text (`ﬁ` → `fi`, full-width `Ａ` → `A`, `²` → `2`). The result
  may contain characters a tokenizer would have split at (`⑴` → `(1)`); it is not re-tokenized.
- **lowercase**: the Unicode default lowercase mapping (`str::to_lowercase`), locale-independent, with the
  `Final_Sigma` rule applied inside the token (`ΟΔΟΣ` → `οδος`). Lowercasing is not case folding: `ß` stays `ß` and
  `ẞ` becomes `ß` (the `fold` filter maps both to `ss`). Turkish and Azeri casing is not tailored: `I` → `i`,
  `İ` → `i̇` (`i` + U+0307, which `fold` reduces to `i`) and `ı` stays `ı`; a Turkish analyzer would need its own
  filter.
- **fold** (accent/diacritic folding): every character outside a per-filter *preserved* set is replaced by its
  canonical decomposition without the combining marks U+0300–U+036F (Latin, Greek and Cyrillic accents: `é` → `e`,
  `ü` → `u`, `ç` → `c`, `ǖ` → `u`, `ά` → `α`, `ё` → `е`), and the result is recomposed (NFC), so marks of other
  scripts and Hangul syllables are unchanged. Letters with no decomposition fold by a fixed table, also when a
  decomposition yields them (`Ǣ` → `AE`): `ß`/`ẞ` → `ss`, `æ` → `ae`, `Æ` → `AE`, `œ` → `oe`, `Œ` → `OE`, `ø` → `o`,
  `Ø` → `O`, `đ`/`ð` → `d`, `Đ`/`Ð` → `D`, `ł` → `l`, `Ł` → `L`, `þ` → `th`, `Þ` → `TH`, `ħ` → `h`, `Ħ` → `H`,
  `ı` → `i` (nothing else: `ŧ`, `ƀ`, `ɨ` and the like are kept). Preserved characters are kept as listed, also
  inside a decomposition, and only the text between them is recomposed (case matters: preserving `ñ` does not
  preserve `Ñ`). The preserved set of each built-in analyzer is part of its definition (§6.3.3); preserving `ñ`
  keeps `año` and `ano` distinct. A preserved character only matches as listed, so put `nfkc` first: a decomposed
  `n` + U+0303 is not a preserved `ñ`. Folding is idempotent.
- **apostrophe**: replaces the typographic apostrophes `‘` (U+2018) and `’` (U+2019) with `'` (U+0027). UAX #29
  joins letters across all three (`don’t` is one token), but the English stemmer knows only U+0027.
- **stop**: removes tokens whose text is exactly a word of a list. The embedded lists (English, Spanish, Portuguese, French, German, Italian)
  are lowercase NFC with their accents, so `stop` runs after `lowercase` and before `fold`; a custom list is matched
  as given. The lists hold closed-class words only: articles, prepositions, conjunctions and negation in all of them;
  English keeps the classic 33-word list, while the others also list personal pronouns, possessives,
  demonstratives and the commonest forms of the auxiliaries (`ser`, `estar`, `haber`; `être`, `avoir`; `sein`,
  `haben`, `werden`; `essere`, `avere`), leaving out words that are just as often content words (`bajo`, `ante`,
  `era`; `or`, `car`, `été`; `stato`, `più`). The normative lists are `persia_analysis::ENGLISH_STOPWORDS`,
  `SPANISH_STOPWORDS`, `PORTUGUESE_STOPWORDS`, `FRENCH_STOPWORDS`, `GERMAN_STOPWORDS` and `ITALIAN_STOPWORDS`, pinned
  by a fingerprint test. German leaves out the modal verbs (`kann`, `will`, also a
  noun) and keeps `sein` and `man`, which are nouns far less often than function words; French keeps `est` (also
  "east") and the possessive `son` (also "sound"), Italian drops `sei` (also "six"). In `fr` and `it`, `elision`
  runs before `stop`, so `c'est` and `l'ho` are the stopwords `est` and `ho`.
- **elision** `{prefixes}`: removes a leading listed word and the U+0027 after it, when something follows
  (`l'été` → `été`; `l'` alone is kept). The embedded lists (`persia_analysis::FRENCH_ELISIONS`, `ITALIAN_ELISIONS`)
  hold the prefixes the French and Italian stemmers remove (`stem` below) plus, for French, the conjunctions ending in `qu`
  (`lorsqu'`, `puisqu'`, `jusqu'`, `quelqu'`, `quoiqu'`). It runs after `apostrophe` (`l’été`) and before `stop`.
- **stem** `{language}`: replaces the token text with its stem, by the Snowball algorithm of the language
  (snowballstem.org, `algorithms/<language>.sbl` of Snowball v3.1.1): `english` (Porter2), `spanish`,
  `portuguese`, `french`, `german`, `italian`. The Snowball test vocabularies (`voc.txt` → `output.txt` of snowball-data at commit
  `019921c3a41c2b70063ee3feb215a00c8e594597`, checked in with the stemmer tests) are normative: every word stems to
  its listed stem. The algorithms expect lowercase text with its accents, so `stem` runs after `nfkc` (to them a
  decomposed accent is a consonant), `lowercase` and `stop`, and before `fold`. English treats only U+0027 as an
  apostrophe (the `en` analyzer maps `‘` and `’` to it first, §6.3.3) and leaves words of fewer than 3 characters alone;
  Spanish drops the acute accents of `á é í ó ú` and keeps `ñ` and `ü`; Portuguese keeps its accents except that a
  final `ç` becomes `c`, and, as it writes `ã` and `õ` as `a~` and `o~` while stemming, turns a literal `a~` or `o~`
  into `ã` or `õ` (only the `whitespace` and `keyword` tokenizers keep `~` inside a token). Stemming is not
  idempotent (`accelerated` → `acceler` → `accel`), so a query is stemmed once, by the same analyzer as the field. A
  stem can be empty (`''s`), which removes the token. French and Italian remove a leading
  elided article, pronoun or preposition before U+0027 (French `c' d' j' l' m' n' s' t' qu'`; Italian `l' d' dell'
  nell' un'` and the others of `italian.sbl`) when something follows it; French writes `ë` and `ï` back after
  stemming; German writes `ß` as `ss` and `ae`, `oe`, `ue` as umlauts (not in `qu`), then removes the umlauts from
  the stem; Italian reads acute accents as grave ones and keeps them.
- **edge_ngram** `{min, max}` (1 ≤ `min` ≤ `max`, in characters: Unicode scalar values, not grapheme clusters):
  replaces a token with its prefixes of `min..=max` characters, shortest first; a token shorter than `min` produces
  none. With `preserve_original`, the whole token follows them when it is longer than `max` or shorter than `min`.
  Used for optional prefix indexing. It stacks tokens, so it applies at index time only (§6.3.4).
- **synonyms** (query time only, §7.6 L1): not a filter of the token stream but a lookup over it. Each term of every
  group (§6.2.1) is analyzed with the field's query-time analysis (§6.3.4) into a token sequence; a term that yields no tokens is ignored,
  and terms equal after analysis are one term. A term whose groups all shrink to it alone (`["New York", "new york"]`
  under lowercasing) has nothing to expand to and never matches. Scanning the analyzed query left to right, the
  longest term starting at each token matches when its texts are equal and its position gaps equal (a stopword gap
  must line up), the matched tokens are skipped, and the match reports, in group order, then term order, the other
  terms of every group containing it, each once, up to a cap (default 50, §7.6 L1). Matches never overlap; the query
  compiler ORs the match with its alternatives.

`nfkc`, `lowercase` and `fold` may lengthen a token (`ﷺ` becomes 18 characters, `ß` → `ss`), so an analyzer that
bounds token length puts the length guard (§6.3.1) after them, and before `edge_ngram` so a token's prefixes stay
bounded.

#### 6.3.3 Built-in analyzers
Every built-in analyzer is at version 1 (`persia_analysis::BUILTIN_ANALYZERS`; `builtin_analyzer(name, version)`
builds one). Built-in names are, and will stay, of the shape `[a-z][a-z0-9_-]*`. Each chain below is the whole definition of that version, with the tables and Unicode version its
filters name (§6.3.2); `guard` is the length guard of §6.3.1 at its defaults (255 bytes, `truncate`), last in every
chain so it bounds the final terms.

| Analyzer | Tokenizer | Token filters, in order |
|---|---|---|
| `standard@1` | `standard` | `nfkc`, `lowercase`, `guard` |
| `keyword@1` | `keyword` | `guard` |
| `whitespace@1` | `whitespace` | `guard` |
| `en@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, `stop` (English), `stem` (`english`), `fold` (preserves nothing), `guard` |
| `es@1` | `standard` | `nfkc`, `lowercase`, `stop` (Spanish), `stem` (`spanish`), `fold` (preserves `ñ`), `guard` |
| `pt@1` | `standard` | `nfkc`, `lowercase`, `stop` (Portuguese), `stem` (`portuguese`), `fold` (preserves nothing), `guard` |
| `fr@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, `elision` (French), `stop` (French), `stem` (`french`), `fold` (preserves nothing), `guard` |
| `de@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, `stop` (German), `stem` (`german`), `fold` (preserves nothing), `guard` |
| `it@1` | `standard` | `nfkc`, `lowercase`, `apostrophe`, `elision` (Italian), `stop` (Italian), `stem` (`italian`), `fold` (preserves nothing), `guard` |

`keyword` and `whitespace` do not normalize: case and accents are significant. The guard truncates each token, so
under `keyword` (one token per value) two values that share their first 255 bytes are one term, and under
`whitespace` two words that do; exact matching of long values is what the `keyword` field type is for (§6.1, §6.4).
`es` keeps `ñ` because it tells words apart (`año`, `ano`); `ü` and the acute accents fold, so `pingüino` and `pinguino` are one term. `en` maps `’` before
stemming, so `John’s` and `John's` both stem to `john`; `fr` and `it` map it too, as their elisions (`l’été`, `nell’estate`)
are written with `’` as often as with `'`, and `de` for its `'s` endings (`Grimm’sche`). Stemming before folding means a query typed without accents
can stem differently from the accented word (`maçã` and `maca` in `pt`); the relaxation cascade (§7.6) is what
bridges such misses. A test pins every built-in's output on a fixed text, so a change to a chain cannot ship as the
same version.

#### 6.3.4 Query-time analysis
Queries (§7) and synonym terms (§6.3.2) are analyzed with the field's analyzer **without the filters that stack
tokens** (`edge_ngram`; `TokenFilter::stacks_tokens`): an index-time prefix such as `lapt` must match the query term
`lapt` as is, and a query or synonym term expanded into prefixes could never line up with an analyzed query. Every
other filter applies at both times, in the same order, so terms meet in the same form. Without `preserve_original`, a query
word longer than `max` analyzes to the whole word, which was never indexed, and no level of the cascade (§7.6)
shortens it: L3 expands it as a prefix (`laptop*`), which no prefix of at most `max` characters matches. A field
that expects longer query words sets `preserve_original` or a larger `max`. No built-in analyzer stacks
tokens, so for them query-time and index-time analysis are the same.

### 6.4 Documents
Documents are submitted as JSON (or typed structs in SDKs) and stored as CBOR in `DocStore` (ADR-0023).

#### 6.4.1 Values and limits
A document is an `id`, an optional `blob` reference (§9.1) and a value per field it has; every field is optional.

| Field type | Value | JSON form |
|---|---|---|
| `text`, `keyword` | a string, or a list of strings (multi-valued) | a string, or an array of strings |
| `i64` | a signed 64-bit integer | an integer in the `i64` range (no fraction or exponent; `-0` is 0, and so is a negative zero written with a fraction or exponent, which JSON parsers cannot tell apart) |
| `f64` | a finite `f64`; `-0.0` is stored as `0.0` | any number (integers are converted to the nearest `f64`) |
| `bool` | `true` or `false` | `true` or `false` |
| `datetime` | microseconds since the Unix epoch, UTC, from `0000-01-01T00:00:00Z` to `9999-12-31T23:59:59.999999Z` | an integer (microseconds) or an RFC 3339 string with a `Z` or `±hh:mm` offset, at most 6 fractional digits and seconds `00`–`59` (no leap seconds); written as RFC 3339 in UTC, `YYYY-MM-DDTHH:MM:SSZ` with `.ffffff` when the microseconds are not zero |
| `stored` | any JSON value | any JSON value |

- Only `text` and `keyword` fields are multi-valued: numeric, `bool` and `datetime` doc values hold one value per
  document (§5.2.1). A list keeps its order; an empty list is a value (it indexes nothing).
- `null` as a field's JSON value means the field is absent, as does `"blob": null`; `null` inside a `stored` value
  is kept. A `null` inside a list of strings is an error, and so is a `null` stored value in a typed API.
- **Floats (`f64` fields).** NaN and the infinities are rejected (JSON cannot express them; typed APIs reject them).
  `-0.0` becomes `0.0` when the value enters a document, so the order-preserving key of §5.2.1 never sees `-0.0` or
  NaN and a range filter on either side of zero includes zero. Numbers inside a `stored` value are kept as parsed:
  an integer in `-2^63..2^64` exactly, any other number as the nearest `f64` (so `-0` reads as `-0.0`).
- **Keys.** `id` and `blob` are the document's id and blob reference. Any other key is a field name. A key the
  schema does not have is an error when the collection is `strict` (the default, §6.2.1). Otherwise it is kept as an
  extra `stored` value under that name, which must satisfy the field name rules of §6.2.1. Duplicate keys at any level
  are errors.
- **Limits.** `id` is 1–512 bytes of UTF-8. A `keyword` value is at most 32 KiB. A document has at most 1024
  fields, schema fields and extra fields together. A `stored` value nests arrays and objects at most 64 levels
  deep. A document holds at most 1 048 576 values: each field value, each element of a list, and each value inside
  a `stored` or extra value (array elements and object members, at any depth) counts once. The JSON text is at most
  16 MiB, and so is the binary encoding (§6.4.2) of the whole document.
- **Output.** `get` writes an object with `id`, then `blob` if present, then the fields and extra fields in
  ascending byte order of their names; values in the forms above.

#### 6.4.2 Binary encoding
The encoding of a document in `DocStore` (and anywhere else a document is persisted) is CBOR (RFC 8949) restricted so
that each document has exactly one encoding (ADR-0023):
- definite lengths only; every argument in its shortest form; no tags; text strings are valid UTF-8;
- the only simple values are `false`, `true` and `null`; floats are always 64-bit (`0xfb`), never 16- or 32-bit;
- map keys are strictly increasing: unsigned integers by value, text strings by their UTF-8 bytes.

Anything else is corruption, and so is a value the rules of §6.4.1 reject.
```
array(5) [
  version   uint    1 (0 is corruption; above 1 is unsupported, read before the array length is checked,
                    so a later version may change it)
  id        text
  blob      bytes (exactly one BlobRef encoding, §9.1) or null
  fields    map: uint field id -> value, ids of the collection's schema
  extra     map: text name -> JSON value (non-strict collections, §6.4.1)
]
```
Values by field type: `text`/`keyword` a text string, or an array of text strings for a list; `i64` and `datetime` an
integer; `f64` a float that is neither NaN, an infinity nor `-0.0`; `bool` a simple value; `stored` and `extra` a
JSON value other than `null` (an absent value is left out): `false`, `true`, an integer in `-2^63..2^64`, a finite
float, a text string, an array, or a map with text keys, whose elements and values may also be `null`.
`DocStore` rows encode the stored fields only (`stored: true`, §6.2.2); the id, the blob reference and the extra
fields are always included. A reader decodes a row with the collection's current schema: evolution keeps
field ids and types (§6.2.4). An extra field named like a field added to the schema later stays an extra field
(but an extra field next to a value of the schema field of that name is corruption). So a row may hold what a new
document could not (such an extra field, or extra fields of a collection that has since become `strict`): it is
returned as it is, and copied as bytes, never re-validated.

---

## 7. Search

### 7.1 Query model
```jsonc
{
  "q": "zapatillas rojas nike",          // user text, analyzed per field
  "fields": ["title^3", "body"],          // default: all text fields with schema boosts
  "filter": { "and": [ {"term": ["brand","nike"]}, {"range": ["price", {"lte": 100}]} ] },
  "sort": "score",                        // or [{"field":"price","order":"asc"}]
  "limit": 20, "offset": 0,               // or "search_after": cursor
  "facets": ["brand"],
  "relax": { "min_results": 1 },          // overrides collection default
  "explain": false
}
```
Programmatic query AST (term, phrase, prefix, fuzzy, bool must/should/must_not, filter, boost, constant_score)
is the internal form; `q` text is compiled to it (SPEC §7.6 / default operator AND).

### 7.2 Scoring: BM25F
Per term `t` and doc `d` over fields `f` with weights `w_f`:
```
tf~(t,d) = Σ_f  w_f · tf(t,d,f) / (1 − b_f + b_f · len_f(d)/avglen_f)
score(t,d) = idf(t) · tf~ · (k1+1) / (k1 + tf~)
idf(t) = ln(1 + (N − df + 0.5)/(df + 0.5))
```
Total = Σ_t score + static_rank contribution. `N`, `df`, `avglen` are **collection-level**, summed across live segments
(and across shards when `global_stats: true`, SPEC §11.5). They are summed from each segment's stored statistics
(§5.1, term `df`, field statistics), which count the segment's deleted documents too until a merge drops them
(§5.5), as in Lucene: a merge of segments without deletions leaves every score unchanged, and one that drops deleted
documents may move scores toward what the live documents alone give (ROADMAP 6.5.5, 7.2.1). Norms are quantized; implementation MUST match the reference
(`persia-engine/tests/reference`) within a documented epsilon.

### 7.3 Execution
- Per segment: build term iterators → boolean combination → collect top-k with **block-max WAND** (MAXSCORE as fallback).
- Filters evaluate to roaring bitmaps (or column predicates) intersected with candidates; tombstones applied last.
- Segments are searched in parallel (thread pool, configurable); per-segment top-k heaps merge into the global top-k.
- Memtable is searched like a tiny segment (near-real-time visibility after `put` returns).

### 7.4 Sorting, pagination
Sort by score or by a column. `offset/limit` supported; `search_after` cursor `(score, segment_gen, docid)` preferred for deep paging.

### 7.5 Facets
Count over a keyword column for the matching bitmap (exact, per segment, then merged). Top-N values returned.

### 7.6 Zero-results avoidance (default on)
If the number of hits `< relax.min_results` (default **1**), the engine runs the **relaxation cascade**,
stopping at the first level that satisfies `min_results` (or continuing to fill `limit` if `relax.fill = true`):

| Level | Name | Behavior |
|---|---|---|
| L0 | `exact` | All query terms required (AND), analyzed |
| L1 | `synonyms` | Expand terms with collection synonyms (OR within group; cap expansions per matched term, default 50; §6.3.2) |
| L2 | `drop_terms` | `minimum_should_match` steps over terms ordered by ascending IDF-importance: 100% → 75% → 50% → 1 term. Stopwords/low-IDF terms dropped first |
| L3 | `prefix` | Prefix-expand the last term (and others if len ≥ 3) via FST range scan (cap expansions, default 50) |
| L4 | `fuzzy` | Edit distance 1 (len 4–7) / 2 (len ≥ 8) via Levenshtein automaton ∩ FST; corrected terms reported |
| L5 | `fallback` | Per collection: `static_rank` (top by static rank / recency), `none` |

Rules:
- Results from a stricter level always rank above results from a looser level (score offset per level).
- The response MUST state what happened:
```jsonc
"relaxation": { "level": "fuzzy", "applied": ["drop_terms:75%","fuzzy"],
                "original": "zapatilas rojs", "effective": "zapatillas rojas",
                "corrected": {"zapatilas":"zapatillas","rojs":"rojas"} }
```
- Filters are **never** relaxed unless `relax.filters = "last_resort"` is explicitly set (default: never).
- Relaxation has a time/work budget (`relax.budget_ms`, default 50 ms); exceeding it returns best-so-far with `truncated: true`.
- Disable per query with `relax: false`.

### 7.7 Highlighting / explain (optional, post-MVP)
`explain: true` returns per-term BM25 contributions and which relaxation level matched. Highlights are post-v1.

---

## 8. Write path and lifecycle

### 8.1 Operations
`put(doc)` (upsert by id), `delete(id)`, `put_batch`, `commit()`/auto-commit policy, `get(id)`.

### 8.2 Flow
1. Op appended to the **WAL** (batched, group commit). Durability level per call: `Buffered | Async | Fsync`, default `Fsync`
   (§8.2.1); a commit syncs whatever is still buffered.
2. Op applied to the **memtable** (visible to searches).
3. When memtable exceeds size/age threshold (default 64 MiB / 10 s idle), it is **flushed** into a new segment (frame/object).
   Writes that never pause for 10 s and stay under the size are flushed by the auto-commit interval (§14
   `commit_interval`, ROADMAP 6.3.4), which bounds the WAL that recovery replays.
4. A **commit** publishes a new manifest (local: SPEC §4.5; remote: SPEC §10.4).
5. WAL chunks fully covered by committed segments are dropped (`wal_start` advances).

Locally (ROADMAP 6.3, ADR-0029) a flush and its commit are one step, and it flushes every memtable that holds
operations, each into one segment of its live documents in docid order (none if every document was replaced or
deleted), with the tombstone sidecars that delete the older segments' copies of the ids it touched (§8.3), whatever triggered it: `commit()`; a memtable at its size threshold, before the next write to it; or
`tick()`, which the embedding calls periodically (commits stay on the caller's thread; merge workers only
merge, ADR-0032) and which commits when any memtable is idle or reached `commit_interval` since its first unflushed
write. Operations replayed at an open count as
written at the open (their age is not logged), so a replayed backlog waits up to one more interval. Flushing all of them makes the
commit's `wal_start` the end of the log, so a crash never replays a record that is already in a segment. A commit builds every segment before it writes anything, so a failure there changes nothing; a failed
write, sync or commit after that poisons the database until it is reopened (§4.5). Creating a collection is a commit
of its own, so its records follow it in the log.

#### 8.2.1 WAL chunks (ADR-0028)
The WAL is one ordered log of operations for the whole database (single writer, §8.4), stored as **chunks**: locally
each chunk is the payload of one `WalChunk` frame (§4.4), remotely one object (`wal/<seq:020>.wal`, §10.4; M12).
Operations are in log order: chunk order, then record order within a chunk.
```
0   4  magic          bytes "PWAL" (0x50 0x57 0x41 0x4C)
4   4  version        u32 = 1
8   4  record_count   u32  (≥ 1)
12  ..  records, record_count times:
          4  body_len    u32  (≥ 3)
          ..  body:
                1  op          1 = put, 2 = delete
                1  name_len    u8, then name_len bytes: the collection name (§6.2.1 rule)
                ..  payload    the rest of the body:
                                 put    the document's encoding with every field (§6.4.2, `FieldSet::All`)
                                 delete the document id, 1..=512 bytes of UTF-8 (§6.4.1)
          4  crc32c      u32  over body_len and body
```
A chunk is at most 64 MiB (the bomb limit its readers pass, §4.4), holds no bytes after its last record, and a put
payload is at most the document limit (16 MiB, §6.4.2). Reading: magic, `version` (0 is corruption; above 1 is
unsupported), then each record's CRC before its body. Anything else that does not hold is corruption: an unknown
`op` (a new op is a new `version`), a name that breaks the rule, an id out of range. A put's document is decoded
during replay, against its collection's schema (below), and a document that does not decode is corruption too.
The per-record CRC is redundant inside a frame, whose own CRC covers it, but not in a remote object.

**Writing** (group commit). The writer encodes records into an in-memory chunk and writes it out (one frame) when
it reaches the target size (default 1 MiB; rotation) or at a sync point; a record that does not fit the 64 MiB limit
next to the buffered ones closes the chunk first. Durability is chosen per call (default `Fsync`):
- `Buffered`: the record stays in the in-memory chunk until the next write-out. A crash loses it.
- `Async`: before the call returns, the in-memory chunk is written to the file, not synced. It survives the process
  dying (the next read-write open syncs what it reads, §4.5.1), not the OS or the machine failing.
- `Fsync`: the chunk is written and synced before the call returns. One sync covers every record written since the
  last one, `Buffered` and `Async` ones included, and a batch of records (`put_batch`) takes one sync; chunks may
  still rotate inside it, so a crash may keep a prefix of a batch.
A commit writes and syncs the in-memory chunk before its Manifest (§4.5 step 1), and only then takes its
`wal_start`, so no record written before the commit lands after it. A failed write or sync stops the
writer, like the container it writes to (§4.5): the records it held are lost or not, and only a reopen tells.

**What may be logged.** Replay must be able to apply every record, or every later open fails. A record is
logged only for an operation that replay can apply: its collection is in the committed manifest (a created
collection's commit completes before its first record), and its collection's memtable accepts it (a put's
document validates and indexes: a docid left, no term over the limit a custom analyzer could emit). The write path must guarantee this
before acknowledging (ROADMAP 6.3.9); the WAL itself checks only the record's framing.

The write path does it in three steps (ADR-0029): the memtable *stages* the operation (validates and indexes a put,
interns the id, but publishes nothing: no view sees it), the record is logged, and only then the memtable *publishes*
it. A rejected operation is never logged, and no reader sees an operation that was not logged. A single `put` or
`delete` is logged with its durability before it is published, so it is visible only once durable at that level; in
a `put_batch` the documents before the last are logged `Buffered` and visible at once, and the last one carries the
batch's durability (one sync for all). A batch whose document is rejected keeps the ones before it, made durable at
the batch's level. A sync that fails after a reader saw a buffered document poisons the database; a reopen decides
what survived.

Schema changes are the other way a logged record could stop applying: replay decodes with the committed schema, and a
put of a non-`strict` collection that kept extra fields fails validation once the collection is `strict` (§6.2.4).
So a commit that changes a schema flushes every memtable first, like a drop: its `wal_start` is the end of the log,
and no record encoded under an earlier schema is ever replayed (ROADMAP 6.3.10). The writer then opens the
collection's segments again with the new schema, so a field added since reads as empty in them; snapshots taken
before keep the schema and segments they hold.

**`wal_start`** (§8.2 step 5) is the start of the oldest chunk that holds a record of a collection not yet covered by
a committed flush; with no such chunk, the end of the log. A flush starts from a point in the log (when its memtable
stops taking writes) and covers the records of its collection logged before that point; records logged while the
flush runs keep holding `wal_start` back. The writer counts a flush once its segment is written, before the commit
that publishes it takes its `wal_start`; if that commit fails, the writer stops (§4.5), so nothing builds on it. Chunks from `wal_start` on may therefore hold records already in committed
segments (of another collection's chunk, or of the same chunk); replaying them is harmless (below).

**Schemas and collections.** A put is encoded with its collection's schema when it is written. A schema change is
committed (§8.7.2) before any record that uses it; since an evolved schema decodes the encodings of every earlier
version (§6.4.2), replay decodes every record with the schema of the manifest it starts from. A commit that drops a
collection flushes every memtable first, so its `wal_start` is the end of the log and no record of the dropped
collection, or of an earlier collection of the same name, is ever replayed. Replay treats a record of a collection
the manifest lacks as corruption.

**Replay** (§8.6) reads the chunks from `wal_start` in log order, the committed ones (`wal_start..log_end`) and the
ones recovery found after `log_end`, and applies each record to its collection's memtable: put as an upsert, delete
as a delete (§8.3). The chunks are whole frames that passed validation, so a torn write never reaches replay: an
invalid chunk or record stops it with corruption, at the first invalid byte, and the open fails. Replay is
idempotent: by id, the state after a put or delete depends only on the last operation on that id, so applying a
suffix of the log again, to a fresh memtable or on top of segments that already hold some of it, gives the same
documents. Replay writes to memtables, and a memtable's version of an id, live or deleted, hides every older
segment's version of it (§8.3), so a segment copy of a replayed id is never visible, and the commit after replay
tombstones it (ROADMAP 6.4.3). A writer's replay also flushes, as writes do (§8.6, ROADMAP 6.6.5).

### 8.3 Upserts and deletes
Upsert = insert into the memtable + tombstones, at the next commit, for every live copy of the id in the committed
segments (looked up via `IdMap`, write path below).
Delete = tombstone only. Tombstones are the only mutable-looking state; they live in per-segment **tombstone sidecars**
(versioned, referenced by the manifest), never inside the immutable segment bytes.
Inside the memtable an upsert replaces in place: the new version gets the next memtable docid and the earlier one is
marked dead, so no search sees it and the flush leaves it out. A delete marks the memtable's version dead and is
recorded even for an id the memtable lacks, so it hides the id in older segments until its tombstone is committed.

Tombstone sidecar (ADR-0030; the layout closes SPEC §18.3):
- One sidecar per `(segment_id, version)`; `version` is a `u64`: 1 for a segment's first sidecar, one more per rewrite
  (§8.7.2). It holds the **full** set of the segment's deleted docids, not a delta, so one sidecar is enough to read;
  each version is a superset of the previous one (deletions are never undone), which the writer checks before
  committing it.
```
0   4  magic       bytes "PTMB" (0x50 0x54 0x4D 0x42)
4   4  version     u32 = 1 (the encoding)
8   16 segment_id  u128
24  8  sidecar     u64  (the sidecar's version, ≥ 1)
32  4  doc_count   u32  (the segment's, 1..=2^31)
36  4  deleted     u32  (1..=doc_count)
40  ..  the deleted docids: a roaring bitmap, portable serialization
..  4  crc32c      u32  over every byte before it
```
- Reading: the length (at most 2^28 + 2^20 bytes, which bounds the writer's encoding of any segment: an encoding
  above it is corruption), the magic, the CRC32C trailer, then `version` (0 is corruption;
  above 1 is unsupported), then the rest. Anything else that does not hold is corruption: `sidecar` 0, `doc_count`
  or `deleted` out of range, a bitmap that does not deserialize or does not end at the trailer, a cardinality other
  than `deleted`, a docid `>= doc_count`. The bitmap's container kinds and offsets are free (the writer uses run
  containers where they are smaller); nothing hashes a sidecar. Opening a database checks `segment_id`, `sidecar`,
  `doc_count` and `deleted` against the segment's manifest entry, as corruption.
- Stored locally as a `TombstoneSidecar` frame (SPEC §4.4), remotely as `tombstones/<collection>/<segment_id>.<ver>.tomb` (SPEC §10.4).
- The manifest maps each live segment to its current sidecar version (absent = no deletions). Superseded sidecars become
  garbage once no snapshot references them (same lifetime rule as segments, SPEC §8.5).

**Write path** (ROADMAP 6.4.3). A flush commit (§8.2) tombstones, for each memtable it flushes, every live copy in
the collection's committed segments of every id the memtable has an operation for (put or delete, replayed or not):
an upsert's older version and a deleted document alike. Each segment that loses documents gets its next sidecar
version, appended after the new segments and before the WAL's commit point and the manifest (§4.5 step 1), and an
updated entry in the commit's diff. A segment that would lose its last live document gets no sidecar: the diff removes it, at
the sidecar version it has (0 for none), and it is obsolete from that commit on, like a merged-away segment (§8.5,
ROADMAP 6.4.6). So a commit leaves at most one live copy of each id, and every snapshot shows
each id at most once: the memtable's version or delete before the commit, the new segment and the sidecars after.

**Lookup** (ROADMAP 6.4.1): the memtable's version or delete of an id hides every segment's; otherwise the newest
segment whose `IdMap` holds the id (§5.3.6) decides: its copy if live, none if its sidecar deletes it. Older copies
are never consulted, so a deleted id stays deleted whatever older segments hold.

### 8.4 Concurrency
- **Single writer** per database/shard (file lock locally; lease remotely, SPEC §11.3). Opening a second writer fails fast.
- Readers in other processes open the database read-only (§8.6): no lock, a fixed snapshot of what the open found.
- Any number of readers; readers see a consistent manifest snapshot (MVCC by manifest `seq`).
- The memtable has one writer too. A reader's memtable view holds exactly the operations applied before it was
  taken (docids and operations are numbered, and a replaced or deleted document records the operation that ended it),
  and a later write never changes what the view returns. Readers take the memtable's lock only while copying data
  out, never for a whole search.
- **Snapshots** (§4.5 step 4, ROADMAP 6.3.3): once a commit's slot is durable, the writer swaps in a new snapshot
  atomically: the manifest, each collection's open segments and a read handle on its memtable. Readers load the
  current one without locks and keep it as long as they like; it shows the committed segments plus the memtable's
  writes as of each view taken from it. A memtable a commit flushes stays in the older snapshots and takes no more
  writes, so every snapshot shows a flushed document exactly once: in the memtable before, in the segment after.

### 8.5 Compaction (merge policy)
Tiered: group segments by size tier (factor 10), merge when ≥ 8 in a tier or when deleted ratio > 20%.
The policy is a pure function (ADR-0031, ROADMAP 6.5.1): a collection's live segments as the manifest lists them,
and the ones a running merge already holds, in; disjoint merge plans out. Each plan lists its input segments in
manifest order, with the sidecar version it read (§8.7.2). For a segment, `live = doc_count - deleted` and its
**live size** is `size × live / doc_count` (rounded down; the manifest has no other size). Segments a running merge
holds are left out. Then, in this order:
1. **Drop**: every segment with `live = 0` goes in one plan that writes no segment (it only removes them). A
   flush commit already removes the segments it leaves without a live document (§8.3); this plan covers the ones
   earlier builds kept listed.
2. **Tier merges**: a segment's tier is 0 when its live size is below `tier_floor × tier_factor`, and otherwise the
   largest `t` with `live size ≥ tier_floor × tier_factor^t`. Segments whose live size is at least
   `max_merged_bytes` take no part. Within each tier, from tier 0 up: while at least `segments_per_merge` segments
   are left, the `segments_per_merge` smallest (by live size, then manifest order) form one plan, if their live
   sizes add up to at most `max_merged_bytes` and their live documents to at most `max_merged_docs`; otherwise the
   tier plans no more merges.
3. **Deletion rewrites**: every segment left with `deleted × 100 > doc_count × deleted_percent` is rewritten alone,
   whatever its size.

Plans come in that order, tier merges by tier then by size, rewrites in manifest order. Defaults (SPEC §14):
`tier_floor` 2 MiB, `tier_factor` 10, `segments_per_merge` 8, `deleted_percent` 20, `max_merged_bytes` 5 GiB,
`max_merged_docs` 2^31; a `tier_factor` or `segments_per_merge` below 2, or a `tier_floor` of 0, counts as 2 or 1,
and a `max_merged_docs` above 2^31 as 2^31.
Live sizes are estimates, so a merged segment may come out somewhat above `max_merged_bytes`.
A merged segment lists its inputs' documents in plan order (§5.5) and is added after the kept segments (§8.7.2).
Order does not matter for lookups: a commit leaves at most one live copy of each id (§8.3), and a merged segment
holds only live documents.
Merges run in background threads, are crash-safe (new segment + new manifest commit), and never block reads or writes.
**Scheduling** (ADR-0032, ROADMAP 6.5.3). The writer plans, a pool of `merge_threads` worker threads (§14, default 1)
merges, and the writer commits. Each plan becomes a job holding its inputs as the writer's open segments and their
deletions at the plan's sidecar versions; a worker merges them (§5.5), writes the merged segment, opens it and runs
`Segment::verify` on it, all in memory. At most one job per thread is in flight, finished ones included, so merged
bytes waiting for their commit are bounded too (backpressure); plans that find no room are made again later. With
`merge_threads` 0 a merge runs on the writer's thread, one per step. The embedding drives the writer: a merge step
commits the jobs that finished, then plans and submits; finishing waits for every merge until the policy plans none;
cancelling stops every job in flight at its next field or stage and commits nothing; closing the database cancels
and joins its workers. A **merge commit** appends the merged segment's frame and a manifest whose diff removes the
inputs at the sidecar versions the plan read and adds the merged segment (none for a drop plan), following the commit
protocol (§4.5); `wal_start` stays where the WAL puts it. It is made only if every input is still live at that
version and the schema is unchanged, which the writer checks before writing anything: otherwise (a commit deleted
from an input meanwhile) the result is discarded and the segments are planned again. A merge that fails, the
merged segment's `verify` included, commits nothing, and its inputs are left out of planning until the database is
reopened, so damage is not merged over and over. A merge's output id is reserved while it runs: no flush draws it.
**Obsolete objects** (ROADMAP 6.5.4): a segment a merge commit removes, and its sidecar, the segments and sidecars of
a collection a commit drops (ROADMAP 6.3.10), and a sidecar a later version
supersedes (§8.3), are obsolete from that commit on, but older snapshots, and merges in flight, may still read them.
The writer tracks each with a weak reference to what readers hold of it (the open segment, every instance opened
with an earlier schema of its collection included, and the decoded deletions):
once no snapshot, job or reader holds it, it is unreferenced and may be deleted. Locally it is a frame, which a
container compaction (§4.6) reclaims; a compaction must keep the obsolete frames still referenced. Remotely (M12)
it is an object to delete. Older manifests are not tracked: no snapshot reads its manifest's frame again.

### 8.6 Recovery
On open: pick superblock → load manifest → replay WAL frames from `wal_start` into a memtable (validating CRCs, §8.2.1)
→ ready. Frames in `wal_start..log_end` were committed: an invalid one there is corruption, never truncated, and
so is a break in their `frame_seq` run or a last frame whose `frame_seq` is not `next_frame_seq - 1` (ADR-0013). Only
the scan from `log_end` (§4.4) ends at a torn tail that is truncated, so mapped bytes never change (§4.7).
Recovery MUST be idempotent.

The open sequence (ROADMAP 6.6.1, ADR-0033), the same for a writer and a read-only open:
1. The container's recovery (§4.5.1): header, active slot, the frames of `wal_start..log_end`, the scan from
   `log_end`.
2. The committed manifest (none for `seq` 0): its frame, decoded and checked against the slot and the local layout
   (§8.7.1).
3. Each collection's segments and tombstone sidecars, opened through the container and checked against their
   manifest entries (§8.3, §8.7.1), and an empty memtable per collection.
4. The `WalChunk` frames from `wal_start` on (those after `log_end` included), in log order, each record applied to
   its collection's memtable (§8.2.1). Replay stops at the first Manifest frame after the loaded `log_end` whose
   `seq` is greater than the loaded manifest's, and does not apply the frames after it (below).
5. Ready: the first snapshot shows the segments and the replayed memtables. Replayed operations count as written at
   the open (§8.2).

Nothing before step 4 writes but the container's tail repair (§4.4); a read-only open writes nothing. One
collection's unflushed record holds `wal_start` back for every collection (§8.2.1), so a backlog can be larger than a memtable: a **writer's replay flushes** (ROADMAP 6.6.5). Before applying a
record it commits if the record's memtable reached its size threshold (as a write does, §8.2 step 3), and when a
memtable has no room left for the record (no docid, operation or id left: `SegmentError::Full` or `TooLarge`, which
an empty memtable would not report) it commits and applies the record again. Such a commit flushes every memtable
(§8.2) and is a normal commit (§4.5), except that its `wal_start` is at most the start of the chunk being replayed:
the records from there on may not be in a memtable yet. The records of that chunk before the flush are then replayed
again by the next open, which is harmless (§8.2.1). Records replayed after the last such commit hold `wal_start` back
until a later flush covers them, like records written after the open. An open that fails or crashes during these
commits leaves a state from which the next open reaches the same documents.

A **read-only open** (ROADMAP 6.6.2) runs the same sequence on a container opened read-only (§4.5.1): no lock, no
write, so it opens beside a live writer, any number at once. It cannot flush, and it does not stop at the size
threshold, so memory grows with each collection's unflushed backlog until the memtable's docid limit (about 2^31
operations) fails the open. A read-write open flushes at the threshold (above) and so bounds that memory. It
shows the snapshot it found and does not follow later commits; it is opened again for those. Because it holds no
lock, a writer may commit while the tail is scanned (§4.5.1). The scan can then include that commit's Manifest
frame, after the loaded `log_end`, and WAL frames logged after it — including records of a collection the loaded
manifest does not have. Replay stops at the first such Manifest whose `seq` is greater than the loaded manifest's,
so the snapshot stays the one the open loaded: the committed manifest, plus WAL from `wal_start` up to that
Manifest. WAL between `log_end` and that Manifest was logged before the newer commit and belongs to the loaded
snapshot. Applying what follows the Manifest would mix two commits, or fail the open on a collection the loaded
manifest lacks. A read-write open holds the lock, so nothing commits during its scan; the same stop applies to a
Manifest a crash left after `log_end` (a commit that did not finish, §4.5).

**Verification** (ROADMAP 6.6.3, `persia verify`) is a read-only open plus what no open or lookup checks: every live
segment's whole bytes (`Segment::verify`, §5.5), that no document id is live in two segments of a collection (§8.3),
and that the bytes after `log_end` are clean, a torn tail, or only zeros and stale frames (a lower `frame_seq`,
which a read-write open truncates too; a read-only open classifies them by reading, §4.4); any other corruption there,
which a read-write open refuses or quarantines, fails it. It writes nothing. `persia verify` (like every `persia` command) exits
with status 3 when it fails on corruption and 1 on any other failure (ROADMAP 6.6.7), so a script tells damage from a failed run. It
analyzes replayed records with the built-in analyzers, the only ones a schema can name (§6.2.2).

### 8.7 Manifest
A manifest is the snapshot one commit publishes (ADR-0024): the commit's `seq`, the committing writer's
`fence_token` (§11.3), `wal_start`, and per collection its current schema and live segments, each with its current
tombstone sidecar (§8.3). Locally it is the payload of the Manifest frame the superblock points at (§4.3), and its
`seq`, `fence_token` and `wal_start` equal those of the slot that commits it (`wal_start` is a container offset);
remotely it is `manifests/<seq:020>.mf` (§10.4), where `wal_start` is the `seq` of the first WAL object still needed
(provisional until the remote WAL is designed, M12).
```
0   4  magic             bytes "PMNF" (0x50 0x4D 0x4E 0x46)
4   4  version           u32 = 1
8   8  seq               u64
16  8  fence_token       u64
24  8  wal_start         u64
32  4  collection_count  u32
36  ..  collections, in strictly ascending byte order of their names:
          u32 schema_len, then schema_len bytes: the schema as JSON (§6.2), every field with its `id` and
              every text field with an `analyzer` of the form "name@version"
          u32 segment_count, then the live segments, oldest first (the order commits added them):
            16  segment_id     u128
            4   doc_count      u32
            8   size           u64  (length of the segment's bytes, §5.1)
            ..  location
            1   tombstones     0 = none, 1 = present, followed by:
                  8 version u64, 4 deleted u32, location
..  4  crc32c            u32  over every byte before it
location:  u8 1 = object (key from the ids, §10.4) | 2 = frame, followed by u64 offset, u64 len (whole frame, §4.4)
```
A manifest is at most 64 MiB. Reading it: the length, the magic, then the CRC32C trailer (it ends every version, so
a damaged `version` reads as corruption), then `version` (0 is corruption; above 1 is unsupported), then the rest.
Fields have one encoding each except the schema text: any valid JSON definition (§6.2) that gives every option
explicitly, ids and `"name@version"` analyzers included, in any key order or spacing. Nothing is left to the reading
build's defaults, so the same bytes read the same schema in every build (`"levels": "default"` follows the engine
by design, §6.2.3). A schema option added later is a new manifest
`version` (older readers then report it unsupported rather than corrupt); a newer build reading version 1 gives the
option the behavior from before it existed. `ManifestHash` is XXH3-128 (§4.1) of the bytes as stored: a replica
(§10.5) copies them, never a re-encoding.

#### 8.7.1 Rules
Anything else is corruption:
- `segment_id`s are unique across the whole manifest; `1 ≤ doc_count ≤ 2^31` (§5.4); `size ≥ 1`.
- A frame location has `offset` and `len` multiples of 8, `offset ≥ 12288`, `len ≥ 32`, and `offset + len` fits a
  `u64`. That the frame exists, lies below `log_end` and has the right kind is checked when it is read.
- A tombstone sidecar has `version ≥ 1` and `1 ≤ deleted ≤ doc_count`. A segment without deletions has none.
- The stored schema is valid (§6.2.1–§6.2.3) and gives every option (§8.7); without ids a reader would assign them
  by name order, and without versions bind the current analyzer, silently changing an evolved schema.

The encoding allows both kinds of location in one manifest, but no layout uses that yet: a local container's
manifest holds only frame locations and a remote one only object locations, and their readers treat any other
location as corruption (ROADMAP 6.3.7). Opening a local database therefore checks, as corruption, that the
committed manifest's `seq`, `fence_token` and `wal_start` equal the active slot's, that it holds no object location,
and that each frame location names a valid frame of the right kind (`Segment`, `TombstoneSidecar`) and exactly that
length, ending at or below `log_end`; it also checks each segment's footer `segment_id` and `doc_count` against its
entry. A remote reader (M12) applies the same layout check with object locations only.

**Segment ids.** Writers draw each new `segment_id` at random: 128 bits from a keyed pseudo-random function whose key comes from the
operating system's randomness (ADR-0029). What matters is uniqueness, not unpredictability; an id already in the
manifest is drawn again.
An id is never reused: older snapshots may still reference a segment that was merged away (§8.5), and remote keys
of segments and sidecars (§10.4) are named by it. A manifest can only check uniqueness within itself and against the
manifest a commit builds on (§8.7.2); random ids make a collision with an older, no longer listed segment
negligible (about `n² / 2^129` for `n` segments ever written).

#### 8.7.2 Commits as diffs
A commit is planned as a diff against the manifest it builds on: per collection, create (schema and first segments),
drop, or change (a replacement schema; segments removed, each with the sidecar version it was read at, 0 for none;
new entries for kept segments; segments added), and optionally a new `fence_token` and `wal_start`. Applying it
gives the next manifest, `seq + 1`, and fails unless:
- `fence_token` and `wal_start` do not go below the base's (§11.3, §8.2);
- a created collection does not exist and its schema has its name; a dropped or changed one exists;
- a replacement schema is what evolving the current one into it gives (§6.2.4), ids included;
- removed and updated segments are live in that collection; each id appears once in the diff; an added segment's
  id is in no collection of the base;
- a removed segment's sidecar version is the one the diff names: a merge drops the documents deleted at that
  version, so deletions committed since would come back in the merged segment;
- an updated entry keeps `doc_count`, `size` and location; its sidecar is the next version (`version + 1`,
  `deleted` not lower) or the first (`version = 1`) of a segment that had none, never the current one (two deletes
  planned on one manifest would both write the next version); deletions are never removed. The manifest sees only
  the `deleted` count, so it can reject a lower count but not a new set that drops some docids while keeping the
  count: the writer builds each sidecar version as a superset of the previous one and checks that before it
  commits (§8.3, ROADMAP 6.4.2). An added segment's sidecar, if any, has version 1.

Kept segments stay in their order and added segments follow, in the diff's order. Diffs name segments by id, never
by position, so one planned against an older manifest (a background merge) still applies to a newer one when the
segments it touches are unchanged, sidecars included, and the rest of the diff is still a valid step from the newer
manifest: its `fence_token` and `wal_start`, if given, are not below the newer ones, a replacement schema evolves from
the newer schema, and collections it creates or drops have not been created or dropped meanwhile. Otherwise it fails
and is replanned. A merge diff, which gives no `fence_token`, `wal_start` or schema, conflicts only on its segments. The diff between two
manifests is the one that applies to the first and gives the second (`seq` aside); there is none if no single
commit leads from one to the other.

**Relocation.** A container compaction (§4.6) is not a diff: it moves frames down, so `wal_start` (a container
offset) may go below the current one. Its manifest is the current one with `seq + 1`, every frame location moved
to where the compaction copied the frame (same length), and the new `wal_start`; nothing else changes.

---

## 9. Blobs

### 9.1 Model
Documents may reference one blob (v1): `blob_ref = { blob_id, size, mime, chunk_size, user_meta? }`.
The index NEVER contains blob bytes — only the reference. `blob_id` is the **SHA-256** of the blob's bytes (32 bytes;
hex-encoded, lowercase, wherever it appears as text) (ADR-0002). Persia always computes it from the bytes it receives;
if a caller supplies an expected id, a mismatch is an error. Chunks must not become addressable under a
`blob_id` before it is verified (upload staging: SPEC §18.8). `BlobRef`/`BlobId` are value types in `persia-format` so
that documents and segments can carry them without depending on `persia-blob`.

Fields: `size` is the blob length in bytes (0 is valid); `chunk_size` is the chunk length used to store it (SPEC
§9.2), `1..=64 MiB`, and the chunk count `ceil(size / chunk_size)` fits in a `u32`; `mime` is 1..=255 bytes of
printable ASCII (`0x20..=0x7E`), stored as given (RFC 6838 caps type and subtype at 127 characters each);
`user_meta` is an optional map of UTF-8 strings, at most 32 entries, keys 1..=128 bytes, values 0..=1024 bytes. An
absent `user_meta` and an empty one are the same value. Searchable metadata belongs in document fields, not here.

Binary encoding (ADR-0011), byte-aligned and self-delimiting, embedded in structures that carry their own checksum
(the `BlobRefs` column, DocStore, WAL records), so it has none itself:
```
0   1   version     u8 = 1
1   32  blob_id     SHA-256 bytes
33  ..  size        varint u64
..  ..  chunk_size  varint u32
..  1   mime_len    u8, then mime_len bytes of mime
..  ..  meta_count  varint u32, then per entry: key_len varint u32, key, value_len varint u32, value
```
Entries are sorted by key bytes, strictly increasing, so each value has exactly one encoding. Any rule above that
does not hold is corruption (the chunk-count rule is reported at `chunk_size`), and so is `version` 0, which no
writer produces; a `version` above 1 is unsupported. Writing a new `version` must be gated by an `incompat_flags`
bit or a format major bump (SPEC §15), so a reader rejects the container at open rather than mid-query or
mid-replay; the byte is a safety net. The largest encoding is 37 295 bytes.

### 9.2 Properties
- Content-addressed ⇒ automatic **dedup**, idempotent `put`.
- Chunked (default **4 MiB**), each chunk with CRC32C ⇒ resumable upload, **range reads**, streaming.
- Not compressed (clients' files are already compressed).
- Metadata (mime, filename, tags) lives in the document and is indexed per schema; the engine never inspects blob content.

### 9.3 Local layout (`.persia.blobs`)
Pack file: header + append-only `BlobChunk` frames `{blob_id, chunk_idx, len, crc, bytes}` + periodic `BlobIndex` frames
(`blob_id → [chunk offsets]`) + superblock like SPEC §4.3. GC rewrites live blobs into a new pack (like container compaction).

### 9.4 Remote layout
`blobs/<hex[0..2]>/<hex(blob_id)>/<chunk_idx>` objects (64-character lowercase hex) (or one object per blob below a size threshold, configurable).

### 9.5 Write ordering and GC
1. Write blob chunks (and blob index). Durable.
2. Commit the document referencing it.
A crash between 1 and 2 leaves an **orphan blob** (harmless). GC: mark = blob_ids referenced by live manifests;
sweep = orphans older than a grace period (default 24 h). A referenced blob must never be missing; `verify` checks this.

### 9.6 API
`put_blob(reader) -> BlobRef`, `get_blob(id) -> Stream`, `get_blob_range(id, range)`, `delete` via document delete + GC.
Convenience: `put(doc, blob: Blob::from_path(..))` performs both steps in the right order.

---

## 10. Storage abstraction and cloud backends

### 10.1 Trait
```rust
pub trait Storage: Send + Sync {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource>;
    fn get(&self, key: &Key) -> Result<Bytes>;
    fn put(&self, key: &Key, data: Bytes) -> Result<()>;                 // atomic visibility
    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome>; // commit primitive
    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>>;
    fn delete(&self, key: &Key) -> Result<()>;
    fn capabilities(&self) -> Capabilities; // conditional_put, strong_list, append
}
```
A **container adapter** implements the local single-file layout (SPEC §4) on top of the filesystem and
presents the same logical objects to the engine. A container stores no names, so the names come from the committed
manifest and superblock (ROADMAP 6.3.6, ADR-0029), with the keys of the remote layout (§10.4):
`manifests/<seq:020>.mf` is the committed Manifest frame, `segments/<collection>/<segment_id>.seg` and
`tombstones/<collection>/<segment_id>.<version>.tomb` the frames the manifest locates (`segment_id` as 32 lowercase hex
digits), `wal/<frame_seq:020>.wal` the `WalChunk` frames of `wal_start..log_end`. The view is read-only: a commit
appends frames and writes a slot (§4.5), which no `put` or `put_if_absent` of one object expresses, so writes are
`Unsupported` and it claims no capability. Every read checks that the frame is of the kind and length the location
gives and lies below `log_end` (§8.7.1); anything else is `Corrupt`.

**Keys.** A `Key` is UTF-8, 1–1024 bytes (the S3/GCS limit), made of `/`-separated segments. Every segment is
non-empty (so no leading, trailing or doubled `/`) and is not `.` or `..`; no character is a control character
(U+0000–U+001F, U+007F), `\` or `:` (a Windows drive or alternate data stream). Keys are validated on construction,
so no key can name a parent directory, an absolute path or a drive. Filesystem-specific limits are the local
backend's job (§10.1.1): a segment whose file name is over 255 bytes, and on Windows a file name that is reserved
(`con`, `nul`, …) or ends in `.`, are rejected there as `InvalidInput`. A `KeyPrefix` follows the same rules
except that it may be empty and may end with `/` (its last segment is incomplete, so it may also be `.` or `..`); it
matches keys by plain byte prefix, as in S3. A backend never resolves a prefix as a path: it lists the directory
part (up to the last `/`) and filters names by the rest.
Keys form a flat namespace, as in S3: `a` and `a/b` may both exist. A backend on a hierarchical store (the local
filesystem) must store both, by encoding names so that an object never collides with a directory (§10.1.1). Keys
are opaque bytes to a backend: it returns them exactly as written (no Unicode normalization, such as macOS NFD file
names, and no case folding).

**Semantics.**
- `get` / `get_range` of a missing key: `NotFound`. `get_range` returns a `ByteSource` whose base offset is
  `range.start` (SPEC §4.1: decoders report absolute offsets) and holds exactly `range.end - range.start` bytes.
  An inverted range is a caller error; a range that extends past the object's end, or a backend returning a
  different length than requested, is `ShortRead`, which is corruption: offsets come from committed metadata.
  A transfer the backend knows was cut off (connection dropped, body shorter than its `Content-Length`) is
  `Transient`, detected before the length check.
- `put` replaces the object; readers see the old or the new bytes, never a mix. An acknowledged `put` or
  `put_if_absent` is durable: it survives a crash of the process or the machine (the local backend fsyncs the file
  and its directory before returning, §10.1.1).
- `put_if_absent` returns `Created` or `AlreadyExists`, atomically. A backend without `conditional_put` returns
  `Unsupported` instead of emulating it with a racy check. **`AlreadyExists` is ambiguous**: an attempt that failed
  may have been applied (including a retry inside the backend's SDK, invisible to Persia), so the object found may be
  this writer's own. The backend resolves it before answering: on every `AlreadyExists` it reads the object and
  returns `Created` if the bytes equal `data`, `AlreadyExists` otherwise (manifests embed their writer's fence token,
  SPEC §11.3, so two writers never produce equal bytes). It never reports an unresolved `AlreadyExists`, which the
  commit protocol (§10.4) would take as a lost commit. An **error** from `put_if_absent` (retries exhausted) leaves
  the outcome unknown: the write may have been applied.
- `list` returns every object whose key starts with the prefix, sorted by key bytes. Without `strong_list` a
  just-written object may be missing from it. Names under the prefix that are not valid keys were not written by
  Persia (e.g. a shared bucket) and are skipped.
- `delete` of a missing key succeeds (idempotent).

**Errors.** Every error has one kind: `NotFound`, `Conflict` (a compare-and-swap update lost a race, e.g. renewing
the writer lease, SPEC §11.3; no `Storage` operation returns it yet, and `put_if_absent` reports `AlreadyExists`
instead), `Transient` (throttling, timeouts, 5xx, and a conditional write that conflicts with one still in flight,
e.g. S3's 409 `ConditionalRequestConflict`: the operation may succeed if repeated), `Corrupt` (stored bytes violate
the format, incl. `ShortRead`), `Io` (a local I/O failure), `InvalidInput` (bad key or range) and `Unsupported`
(missing capability). Only `Transient` is retryable; backends classify their native errors into it, so callers
never inspect backend-specific errors.

**Retries.** Backends retry `Transient` errors with a shared policy: at most `max_attempts` attempts (default 5),
waiting before retry `n` (from 0) a uniformly random duration in `0..=min(max_delay, base_delay·2ⁿ)` ("full
jitter"; defaults 50 ms and 5 s). Sleeping goes through an injectable clock and the randomness through a seeded
generator, so tests are deterministic. `put_if_absent` is the exception to blind retries: see its ambiguity rule
above.

### 10.1.1 Local backend layout (object-per-file, ADR-0012)
`file:///…` stores each object as one file under a root directory. A key `s1/s2/…/sn` is the file
`<root>/d(s1)/d(s2)/…/f(sn)`, where `f(s)` is the **encoded** segment and `d(s) = f(s) + "@"` names a directory:
- **Encoding.** Bytes `a`–`z`, `0`–`9`, `.`, `_` and `-` stand for themselves; every other byte of the segment's
  UTF-8 (uppercase letters, space, `%`, `@`, non-ASCII, …) is `%` followed by two **uppercase** hex digits
  (`É` → `%C3%89`, `A` → `%41`). File names are therefore ASCII, so a normalizing filesystem (HFS+ stores NFD) has
  nothing to rewrite, and they stay distinct under case folding (NTFS, APFS): outside an escape no letter is
  uppercase, inside one none is lowercase. A name is decoded only if it is canonical (re-encoding the decoded segment
  gives the name back) and the result is valid UTF-8; any other name was not written by Persia and is skipped.
- **Objects never collide with directories.** A directory name ends with `@`, which an encoded segment never
  contains, so the object `a` (file `a`) and the objects under `a/` (directory `a@`) coexist.
- **Limits.** A file name `f(s)` or `d(s)` over 255 bytes is `InvalidInput` (the common filesystem limit), so a
  segment of plain characters is at most 255 bytes as the last segment and 254 bytes before a `/`. On Windows a name
  that is reserved (`con`, `prn`, `aux`, `nul`, `com1`–`com9`, `lpt1`–`lpt9`, alone or before a `.`, ignoring case)
  or that ends in `.` is `InvalidInput` too. On Unix, so is an object whose file path, or the path of its temporary
  file (below), is longer than `PATH_MAX` allows (4095 bytes on Linux, 1023 on macOS and the BSDs): it depends on the
  root's path, and on macOS and the BSDs even the longest key (§10.1) exceeds it under any root. The root's path is
  measured as given (made absolute, symbolic links not resolved); a root behind a symbolic link whose target path is
  longer can still fail with an I/O error instead.
- **Temporary files** are named `@tmp.<nonce>.<n>` (a random per-process nonce in hex and a counter); they start with
  `@`, so they never decode, live in the directory of the object they become, and are skipped by `list`. A crash may
  leave one behind; it holds no acknowledged data. A `put` holds an exclusive lock on its temporary file (Unix
  `flock`) from just after creating it until the name is gone, and after locking checks that the name still names
  its file, taking another name if not (removing its file first if another handle holds the lock: a listing).
  `sweep_temporaries` removes, only on request (never at `open`), every temporary file whose lock it can take, while
  holding it; on Windows, where files are not locked, it removes none unless forced (ADR-0019, ROADMAP 2.3.8;
  `persia store sweep`). A lock counts only on a local disk file system: where `flock` is emulated with per-process
  locks (Linux NFS, some FUSE mounts), a lock taken may be a writer's of the same process, so the file's use is
  unknown and only a forced sweep removes it (ADR-0019 decision 6).
- **Writes.** `put` writes a temporary file, fsyncs it, renames it over the object and fsyncs the directory.
  `put_if_absent` does the same but hard-links the temporary file to the object name instead of renaming (link fails
  if the name exists, atomically), then removes the temporary file and fsyncs the directory, whatever the outcome
  (an object found may not be synced yet); `AlreadyExists` is resolved as in §10.1. A filesystem without hard links
  therefore cannot run `put_if_absent` (`Io` error). Before writing into a directory for the first time, a store
  creates the missing directories and fsyncs the parent of each one on the path, so an acknowledged object is never
  lost with a directory entry another writer has not synced yet; a name on the path that exists but is not a real
  directory (a file, a symbolic link) is an `Io` error. `open` does the same for the root's missing ancestors and
  the root's own entry; ancestors that already existed are taken as durable. `delete` removes the file and fsyncs its
  directory (an object behind a symbolic link or a file on the path does not exist, so `delete` succeeds without
  touching it); directories are never removed by Persia, so a concurrent `put` never finds its directory gone. A
  directory removed by someone else is created again by the next write into it.
  Directory syncs work on Unix and Windows alike (§4.5.2, ADR-0016).
- **Reads** open the file (a rename never changes an open file, so a read sees one version); on Windows an open
  that a concurrent `put` is replacing can briefly fail with `ERROR_ACCESS_DENIED` (the replaced file is pending
  deletion), and is retried. Reads use positioned reads (`pread`); `get_range` checks the range against the file length first, so a range past the end is
  `ShortRead` without reading it, as is a file found shorter while reading. An entry `list` would skip (a symbolic
  link, also one on the path, a FIFO, a file where a directory of the path should be) reads as `NotFound`. `list` lists the directory named
  by the prefix's directory part (missing, a file or a symbolic link: nothing matches; a missing root is `Io`, so an
  unmounted store never looks empty), keeps the decodable names whose decoded segment starts
  with the prefix's last part, descends into every matching directory, and sorts the keys by bytes. Symbolic links
  and other non-regular entries are skipped. The root's contents are otherwise trusted: another program changing
  files under it is outside this contract.

### 10.2 URL schemes
`persia::open("app.persia")`, `file:///…`, `s3://bucket/prefix`, `gs://bucket/prefix`, `az://account/container/prefix`,
`memory://`. Scheme selects the backend. **Zero Persia-specific configuration** is required when the ambient environment
provides credentials.

### 10.3 Credentials
Use each provider's standard chain: env vars, shared config files, IMDS / IRSA / Workload Identity / managed identity.
Explicit options are overrides, not requirements. Endpoint override supported (MinIO, fake-gcs, Azurite).

### 10.4 Remote commit protocol
Remote layout:
```
<prefix>/
  manifests/<seq:020>.mf        immutable, created with put_if_absent
  segments/<collection>/<segment_id>.seg
  tombstones/<collection>/<segment_id>.<ver>.tomb
  wal/<seq:020>.wal
  blobs/...
  LEASE                          writer lease (SPEC §11.3)
```
1. Upload new segments/WAL objects (any order, immutable, content-named).
2. **Commit = `put_if_absent(manifests/<seq+1>.mf)`**. `Created` ⇒ committed. `AlreadyExists` ⇒ another writer won ⇒ fence/abort (resolved against the writer's own bytes first, §10.1). An error ⇒ outcome unknown: the writer keeps its uploads (they may be referenced by the manifest) and reads `manifests/<seq+1>.mf` to resolve the outcome before retrying or aborting.
3. Readers find the latest manifest by `list` (or a best-effort `CURRENT` hint) and may tail for newer ones.

### 10.5 Local-first replication ("replicated" mode)
`db.replicate("s3://…")`: local file is the primary; a background replicator uploads committed segments
and then manifests in order, tracks the **replicated seq**, and is resumable. Local writes never block on the network.
Remote readers can open the replica read-only (eventually consistent, lag exposed as a metric).

### 10.6 Read path and cache
Remote reads go through a **block cache** (RAM + local NVMe, LRU/ARC, keyed by `(object, block)`), with:
range-request coalescing, readahead for sequential sections, request hedging (optional), and singleflight to
dedup concurrent identical fetches. Segment footers and FSTs are pinned on open.
Above it, an open segment reads its block-framed sections (SPEC §5.2) through a cache of **decoded** blocks keyed by
`(open segment, section, block)`, LRU and bounded in bytes, which any number of segments may share (each opening of a
segment has its own entries, dropped with it): once a block is in it, term lookups, posting lists, norms, columns and
document fetches reuse it without decompressing it or checking its CRC again. Readers that miss the same block at the
same time may each decode it; one copy is kept. Segments opened without a given cache share one per process
(8 MiB, `BlockCache::shared`); the embedded API and the server are to open all their segments with one cache sized
by their configuration (planned: ROADMAP 10.2.5, 11.4.7). The capacity bounds what the cache keeps: a reader keeps
the block it is reading in each stream it reads, also after the cache evicts it.
It holds no state of its own on disk.

---

## 11. Distribution

### 11.1 Model
A **cluster** is a set of **shards** — each shard is an independent Database (own manifest chain) under
`<prefix>/shard-<n>/`. Documents route by `hash(id) mod shard_count` (xxh3). `shard_count` is fixed at collection-set
creation in v1; resharding (split) is a post-v1 operation designed around segment immutability.

### 11.2 Roles (all in `persia-server`, selected by flags/env)
- **writer**: owns one shard's lease, accepts writes, runs compaction, uploads segments.
- **reader**: serves queries from committed manifests; stateless; scales via HPA; warms cache from object storage.
- **router/coordinator** (may be co-located): scatter-gather queries across shards; merges top-k and facets.

### 11.3 Writer lease and fencing
Lease object `LEASE` = `{holder_id, fence_token, expires_at}` renewed periodically via conditional update.
Every manifest embeds `fence_token`. A writer whose lease lapsed MUST stop writing; a stale writer's manifests
are rejected by readers (token regression) and cannot win `put_if_absent` once a newer seq exists.

### 11.4 Replica refresh
Readers poll for the next manifest seq (default 1 s, with jitter) and swap snapshots atomically.
Staleness is exposed as `persia_replica_lag_seconds`.

### 11.5 Cross-shard scoring
Default: shard-local statistics (fast, slightly inexact). Option `global_stats: true`: router gathers `(N, df, avglen)`
per query term from shards in a pre-phase (DFS-style) before scoring.

### 11.6 Kubernetes
Shipped artifacts: OCI image (distroless, non-root, read-only rootfs), Helm chart:
`StatefulSet` for writers (one pod per shard, PVC for cache/WAL), `Deployment` + `HPA` (QPS/CPU) for readers,
headless Service for shard discovery, PodDisruptionBudgets, readiness/liveness (`/readyz`, `/livez`),
graceful shutdown (drain, flush, release lease), `ServiceMonitor`.

---

## 12. Server and API

### 12.1 Transports
gRPC (tonic) is primary; HTTP/JSON (axum) mirrors it for tooling. Both generated from/aligned with `persia.proto`.

### 12.2 Services (sketch)
```proto
service Persia {
  rpc CreateCollection(CreateCollectionRequest) returns (Collection);
  rpc Put(stream PutRequest) returns (PutResponse);        // docs, optional blob chunks
  rpc Delete(DeleteRequest) returns (DeleteResponse);
  rpc Get(GetRequest) returns (Document);
  rpc Search(SearchRequest) returns (SearchResponse);     // includes relaxation info
  rpc GetBlob(GetBlobRequest) returns (stream BlobChunk); // supports ranges
  rpc Commit(CommitRequest) returns (CommitResponse);
  rpc Stats(StatsRequest) returns (StatsResponse);
}
```
Errors map to canonical gRPC codes; all responses carry `manifest_seq` for read-your-writes via `min_seq` on requests.

### 12.3 Auth and transport security
TLS (rustls) optional-on by config; API keys / bearer tokens in v1; mTLS supported. No auth in embedded mode.

### 12.4 Observability
`tracing` spans + OpenTelemetry export; Prometheus `/metrics`
(query latency histograms by relaxation level, cache hit ratio, segments/collection, merge backlog, WAL bytes,
replica lag, blob throughput). Structured JSON logs.

---

## 13. SDKs

Two client families are planned. They are not interchangeable, and neither is implemented. The embedded
Rust facade they would sit beside is still a stub (`crates/persia`).

### 13.1 Embedded bindings (Go, TypeScript)

A Go or TypeScript program opens a `.persia` file in-process, the way the Rust `Db` API will (ROADMAP M10,
SPEC §8, §9.6, §14). The binding strategy is ADR-0018 (proposed): one crate, `persia-ffi`, exposes a C ABI
that Go links with cgo and a safe Rust API that the TypeScript N-API package calls. A later WASM build for
browsers and edge isolates is a separate package and is not the first release. The design is `docs/sdk/`.

These bindings add no on-disk bytes. A file written by Rust, Go, or TypeScript is the same file (§15).
They are blocked on the embedded API (M10). They do not replace the `persia` crate.

### 13.2 Network clients (Rust, Go)

- **Rust** (`sdk/rust`, crate `persia-client`) and **Go** (`sdk/go`, module `persia-go`).
- Generated stubs from `persia-proto` + thin idiomatic layer: connection pooling, client-side load balancing
  (shard-aware for writes), retries with backoff on idempotent ops, deadlines, batching, blob streaming helpers.
- A **shared conformance suite** (language-neutral scenarios in YAML/JSON) runs against both SDKs and the server in CI.
  The embedded bindings (§13.1) reuse that scenario format with a `transport` field once it exists (ROADMAP 14.1);
  they do not wait on the server to test a local file.
- Semantic versioning; SDK ↔ server compatibility matrix documented. The embedded packages version
  separately (ABI, file format, language module); see ADR-0018.

---

## 14. Configuration

Embedded: builder options (`OpenOptions`), sensible defaults, no files.
Server: flags → env (`PERSIA_*`) → optional TOML. Defaults MUST be production-safe.
Key knobs: `memtable_bytes`, `commit_interval`, `fsync_policy`, `merge_threads`, `merge.*` (§8.5), `search_threads`,
`cache_ram_bytes`, `cache_disk_path/bytes`, `block_size`, `blob_chunk_size`, `relax.*`.
Defaults: `memtable_bytes` 64 MiB (with a 10 s idle flush), `commit_interval` 60 s (§8.2 step 3); `merge_threads` 1
(0 merges on the writer's thread); `merge.*` as in §8.5.

---

## 15. Versioning and compatibility

- `format_major` bump = breaking; readers reject. `format_minor` bump = additive: a writer of an older minor may
  still write (and rewrite, SPEC §4.6) a newer-minor container, so anything an older writer could not maintain must
  be gated by a `rocompat_flags` bit instead. Readers never branch on `format_minor`, only on flags (ADR-0008).
- `incompat_flags` for features old readers cannot handle; `rocompat_flags` for read-only-safe features.
- Each segment records its own format version ⇒ mixed-version databases are valid; compaction upgrades gradually.
- Golden files for every released format version live in `tests/fixtures/format/v<major>.<minor>/` and are opened
  by CI forever. `cargo xtask fixtures` writes (and CI checks) only the directory of the version the build writes;
  a released version's directory is never regenerated.
- Encodings that live inside frames or objects rather than in the container layout have their own version field and
  golden files under `tests/fixtures/<encoding>/v<version>/` (manifests: `tests/fixtures/manifest/v1/`, ADR-0024; tombstone sidecars: `tests/fixtures/tombstones/v1/`, ADR-0030),
  checked by the owning crate's tests; missing ones are written on request (`PERSIA_WRITE_MANIFEST_FIXTURES=1`, `PERSIA_WRITE_TOMBSTONE_FIXTURES=1`),
  existing ones never.
- Whole databases written by the engine's commit path, which exercise every encoding inside a container together,
  live in `tests/fixtures/database/v<major>.<minor>/` (container format version), checked by `persia-engine`'s tests
  and written on request when missing (`PERSIA_WRITE_DATABASE_FIXTURES=1`; ROADMAP 6.3.8). `blob-ref.persia` holds a
  segment whose `BlobRef` has `user_meta`, and a WAL record after the last commit; `tombstones.persia` (ROADMAP 6.4.3)
  three segments, two versions of one tombstone sidecar, and a delete in the WAL. `emptied.persia` (ROADMAP 6.4.6): the same
  commits, whose third removes the segment it empties instead of writing its second sidecar.
- A format version is frozen once released (tagged). Until then its layout may be amended in place by an ADR
  (e.g. ADR-0008); every change after release bumps the version and updates fixtures. Until v1.0 there is no
  compatibility promise across released versions.

---

## 16. Quality requirements

### 16.1 Testing (normative)
- Unit + property tests for every primitive and round-trip.
- Fuzz targets for: frame reader, superblock, segment footer, each section decoder, query parser, WAL replay.
- **Differential testing**: random operation sequences (put/delete/commit/merge/reopen/crash) executed against the engine and the
  reference model; results (hit sets, scores within epsilon) MUST match.
- **Crash testing**: fault-injecting `Storage` (torn writes, dropped fsync, partial uploads, reordering) and `kill -9` loops.
  `FaultyStorage` wraps any backend and injects errors, latency, torn and dropped writes, and applied writes whose
  response is lost, on a schedule scripted per call or drawn from a seed, so a failure replays from its seed when
  the calls are made in a deterministic order (e.g. one thread).
  A crash fault models process death: that call (applied or not) and every later one fail. In volatile mode, which
  breaks the durability §10.1 promises on purpose, acknowledged writes stay readable but unsynced until the test
  syncs them, and a crash loses a seeded subset: a suffix of all unsynced writes (an ordered write-back cache, which
  correctly ordered software survives), or a suffix per object (writes to different objects reordered, to check
  that the damage is detected rather than a panic). A crash never undoes an object another writer has replaced since.
- **Storage conformance**: every `Storage` backend passes one shared suite of the §10.1 contract, including a
  model-based random operation sequence and concurrent `put_if_absent` races.
- Integration against MinIO, fake-gcs-server, Azurite via docker-compose.
- Synthetic load + soak tests, relevance regression suite (fixed corpora + judged queries; nDCG@10 tracked).

### 16.2 Performance (targets, to be validated by benchmarks — not promises)
| Metric | Target |
|---|---|
| Query p50 (warm, 10M short docs, 1 node) | < 10 ms |
| Query p99 (warm) | < 50 ms |
| Cold namespace first query (object storage) | < 500 ms |
| Indexing throughput (1 writer) | ≥ 50k docs/s (short docs) |
| Open time (1 GB db, local) | < 100 ms |
| Recovery after crash | proportional to WAL size, < 5 s for 64 MiB WAL |

### 16.3 Benchmarks
Reproducible harness (`benches/`, dataset download scripts, pinned configs), tracked in CI for regressions (> 5% fails).
Comparison against other engines is reported with methodology, never cherry-picked.

---

## 17. Security considerations

- Treat every byte read from disk/network as hostile (fuzzing; bounds-checked parsing; decompression-bomb limits).
- Limits: max doc size, max field count, max query terms, max fuzzy expansions, max result window.
- Path traversal safe (collection/blob names validated); no shelling out.
- Secrets never logged; redaction in debug output of storage config.
- Supply chain: `cargo deny`, `cargo audit`, pinned toolchain, reproducible container builds, SBOM.

---

## 18. Open questions (track as ADRs)

1. ~~Blob hash~~: resolved by ADR-0002, SHA-256 (§9.1).
2. Single blob per document vs many (`blobs: []`) in v1.
3. ~~Tombstone sidecar format vs copy-on-write segment footers~~: resolved by ADR-0030, full-set sidecars (§8.3).
4. Positions: always-on vs opt-in (current: opt-in).
5. Resharding strategy (split-by-hash-range using immutable segments + filtering merge).
6. Vector search layout (IVF-style clusters in segments) — reserve `incompat_flags` bit.
7. Language analyzers scope for v1 (en, es, pt first).
8. Blob upload staging: the id is known only after hashing the last byte, but §9.3/§9.4 key chunks by `blob_id`.
   Proposal to evaluate: stage chunks under an upload id, then bind them to the verified `blob_id` with the
   `BlobIndex` frame (local) or a per-blob index object written with `put_if_absent` (remote); dedup check after
   hashing; resumable uploads re-hash stored chunks or persist hasher state. Also: how an expected id is passed
   (API/gRPC). Must be decided (ADR) before ROADMAP 9.2.
