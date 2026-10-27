# ADR-0028: WAL chunks, durability levels and replay

- Status: proposed
- Date: 2026-10-27
- Related: SPEC §4.4, §4.5, §6.4.2, §8.2 (§8.2.1), §8.6, §8.7.2, §10.4; ROADMAP 6.2.1–6.2.5, 6.3.4, 6.6.1

## Context

SPEC §8.2 named the WAL, group commit and the durability levels `Buffered | Fsync | Async`, and the container already
has `WalChunk` frames (kind 1) and recovers synced frames after `log_end` (§4.4). Nothing fixed what a chunk holds,
when a writer writes or syncs one, how `wal_start` advances with one log and several collections, or what replay may
assume about the records it reads.

## Decision

1. **One log, records in chunks.** The database has one WAL (one writer, §8.4). A chunk is a frame payload locally
   (an object remotely, M12): magic `PWAL`, `version`, `record_count`, then records of `body_len`, body and a CRC32C.
   The body is `op` (put, delete), the collection by name (`u8` length: names are at most 64 bytes, §6.2.1), and the
   payload: a put's full document encoding (§6.4.2), which carries the id, or a delete's id. Fixed little-endian
   lengths, like the manifest (ADR-0024); the per-record CRC is redundant locally but lets a remote chunk be checked
   record by record without a frame around it.
2. **No sequence numbers in records.** Log order is chunk order (`frame_seq` locally, the object `seq` remotely),
   then record order, and nothing reads a record by number.
3. **Durability.** `Buffered` keeps the record in the writer's in-memory chunk; `Async` writes the chunk to the file
   without syncing (it survives a process crash, since a read-write open syncs what it read, §4.5.1); `Fsync` writes
   and syncs, and one sync covers everything written before it (group commit). A batch takes one sync.
   The default is `Fsync`, matching "durability is non-negotiable" (CLAUDE.md pillar 5); `Buffered` and `Async` are
   opt-ins for bulk loads. A chunk is written out at 1 MiB (rotation) or at a sync point, and never exceeds 64 MiB.
4. **`wal_start` per collection.** The writer remembers, per collection with unflushed records, the oldest chunk
   holding one; `wal_start` is the oldest of those, or the end of the log. A flush of one collection does not move
   it past chunks that also hold its records if another collection still needs them.
5. **Idempotent replay instead of skipping flushed records.** Chunks from `wal_start` on may hold records already in
   committed segments. Replay applies them again: puts are upserts and deletes are deletes, so by id only the last
   operation counts, and the visible documents are the same. Skipping them would need a per-collection "flushed up
   to" position in the manifest, a format change for a cost (re-indexing some records at open) bounded by the
   auto-commit interval (§14 `commit_interval`, ROADMAP 6.3.4).
6. **Collections in replay.** Replay decodes every put with the schema of the manifest it starts from: schema
   changes are committed before records use them, and an evolved schema decodes earlier encodings. Dropping a
   collection first flushes every memtable, so the drop's commit has `wal_start` at the end of the log and no record
   of the dropped (or a recreated) collection is replayed; a record for a collection the manifest lacks is
   corruption.
7. **No torn records.** A chunk is a whole frame that passed its CRC, so a crash leaves no partial chunk for replay
   to cut: an invalid chunk or record means a writer bug or damage and fails the open (§8.6). Torn tails are the
   container's business (§4.4). Replay stops at the first invalid byte and reports its offset.
8. **Golden fixtures** live in `tests/fixtures/wal/v1/`, written by an engine test with
   `PERSIA_WRITE_WAL_FIXTURES=1` when missing, never overwritten (as ADR-0024 does for manifests).

## Consequences

- An open replays up to one auto-commit interval of records per collection, some of them already flushed; the
  memtable may need to flush during replay (ROADMAP 6.6.1).
- A drop costs a flush of every collection.
- A schema change costs a commit before the first record that uses it.
- The remote WAL (M12) reuses the chunk encoding; only the object naming and `wal_start` as a `seq` differ.

## Alternatives considered

- **One WAL per collection**: `wal_start` per collection in the manifest and a frame per collection per group
  commit; more syncs for multi-collection batches, and a manifest format change.
- **A sequence number per record, flushed-up-to in the manifest**: exact replay, at the cost of a manifest version
  and of keeping that position right through merges and compaction. Possible later as a manifest version 2.
- **Collection ids instead of names**: the manifest identifies collections by name, and a name is at most 64 bytes.
- **Truncating at the first invalid record** like a torn tail: records inside a CRC-valid frame are not torn by
  crashes, so that would silently drop acknowledged writes on damage.
