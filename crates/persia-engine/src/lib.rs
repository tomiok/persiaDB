//! Persia DB search engine: segments, indexing, WAL, compaction, query, scoring, relaxation.
//!
//! Synchronous and storage-agnostic: all I/O goes through `persia_storage::Storage`.
#![forbid(unsafe_code)]
