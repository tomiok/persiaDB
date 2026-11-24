//! Persia DB: an embedded, single-file, cloud-native search database.
//!
//! This crate is the public embedded API (ROADMAP M10; SPEC §6.2, §7, §8.1, §9.6, §14): open a file with
//! [`Db::open`], and it just works. No daemon, no configuration file.
//!
//! ```
//! use persia::Db;
//!
//! # if cfg!(miri) { return Ok(()); } // real files: Miri's isolation has no filesystem
//! # let dir = tempfile::tempdir()?;
//! # let path = dir.path().join("app.persia");
//! let db = Db::open(&path)?; // creates app.persia if it is missing
//! db.close()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Every call returns [`Result`], whose [`Error`] carries a stable [`ErrorCode`] (ADR-0064). The API is being
//! built leaf by leaf (ROADMAP 10.1, 10.2): collections, documents, blobs and search follow. The engine
//! (`persia-engine`) and the hidden [`internal`] layer under this crate (ADR-0051) are not stable APIs.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// Sizes and offsets convert with `From` or `try_from`, never a silent `as` (CLAUDE.md checked ops).
#![cfg_attr(not(test), deny(clippy::as_conversions))]

mod db;
mod error;
#[doc(hidden)]
pub mod internal;
mod location;
mod options;

pub use db::Db;
pub use error::{Error, ErrorCode, Result};
pub use options::OpenOptions;
/// How durable a write is before its call returns (SPEC §8.2.1): `Buffered`, `Async` or `Fsync` (the default).
pub use persia_engine::wal::Durability;
