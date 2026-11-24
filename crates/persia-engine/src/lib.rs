//! Persia DB search engine: segments, indexing, WAL, compaction, query, scoring, relaxation.
//!
//! Synchronous and storage-agnostic: all I/O goes through `persia_storage::Storage`.
// `deny`, not `forbid`: the read-only open's mapping module (`db/read_only/mapped.rs`) must be able to `allow` it, and
// it alone may (ADR-0017 amended, `UNSAFE_ALLOWED` in `xtask/src/repo_checks.rs`).
#![deny(unsafe_code)]
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
pub mod relax;
pub mod schema;
pub mod score;
pub mod search;
pub mod segment;
pub mod stats;
pub mod tombstone;
pub mod view;
pub mod wal;

/// The analysis crate (SPEC §6.3): the [`AnalyzerRegistry`](persia_analysis::AnalyzerRegistry) a database opens with
/// ([`db::DatabaseOptions::registry`]) and the analyzers a caller registers in it (ROADMAP 6.6.8), for crates that
/// reach the engine only, such as the CLI.
pub use persia_analysis as analysis;
