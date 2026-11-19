//! Persia DB: an embedded, single-file, cloud-native search database.
//!
//! **Placeholder for M10.** This crate will be the public embedded API (`Db`, `Collection`; SPEC §6.2, §7, §8.1).
//! Until then the write path and search are library code in `persia-engine` (`persia_engine::db::Database`), and
//! blobs with documents are the hidden [`internal`] layer (ADR-0051), neither of which is a stable API.
#![forbid(unsafe_code)]
// Sizes and offsets convert with `From` or `try_from`, never a silent `as` (CLAUDE.md checked ops).
#![cfg_attr(not(test), deny(clippy::as_conversions))]

#[doc(hidden)]
pub mod internal;
