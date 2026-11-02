//! Persia DB search engine: segments, indexing, WAL, compaction, query, scoring, relaxation.
//!
//! Synchronous and storage-agnostic: all I/O goes through `persia_storage::Storage`.
#![forbid(unsafe_code)]
// Sizes and offsets convert with `From`, `persia_format::widen` or `try_from`, never a silent `as` (CLAUDE.md checked
// ops, ROADMAP 0.6.5).
#![cfg_attr(not(test), deny(clippy::as_conversions))]

mod bitmap;
pub mod db;
pub mod document;
pub mod manifest;
pub mod memtable;
pub mod merge;
pub mod query;
pub mod schema;
pub mod score;
pub mod search;
pub mod segment;
pub mod stats;
pub mod tombstone;
pub mod view;
pub mod wal;
