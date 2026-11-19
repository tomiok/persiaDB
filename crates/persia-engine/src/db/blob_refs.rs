//! The blob references GC marks (SPEC §9.5, ROADMAP 9.5.1): every `BlobRef` of the live segments, the snapshots and
//! jobs that still hold obsolete ones (SPEC §8.5), the memtables, and the WAL records from `wal_start` on. Filled in
//! by the GC batch.
