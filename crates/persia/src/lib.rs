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
//! A [`Db`] holds [`Collection`]s of [`Document`]s (put as values, as JSON with [`Collection::put_json`], or as any
//! `serde` type with [`Collection::put_struct`]) and their blobs ([`Blob`], [`Db::put_blob`]). Reads see every write
//! that returned; commits and merges run on the database's own thread, and [`Db::close`] joins it (ADR-0065). Every
//! call returns [`Result`], whose [`Error`] carries a stable [`ErrorCode`] (ADR-0064). Search lands with ROADMAP
//! 10.1.3. The engine (`persia-engine`) and the hidden [`internal`] layer under this crate (ADR-0051) are not stable
//! APIs.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// Sizes and offsets convert with `From` or `try_from`, never a silent `as` (CLAUDE.md checked ops).
#![cfg_attr(not(test), deny(clippy::as_conversions))]

mod blob;
mod collection;
mod db;
mod documents;
mod error;
#[doc(hidden)]
pub mod internal;
mod location;
mod maintenance;
mod options;
mod typed;

pub use blob::BlobReader;
pub use collection::Collection;
pub use db::Db;
pub use error::{Error, ErrorCode, Result};
pub use internal::blob::PutBlobOptions;
pub use internal::documents::Blob;
#[doc(hidden)]
pub use options::DetachedSlot;
pub use options::OpenOptions;
// Blob references (SPEC §9.1): plain values (ADR-0064 item 2).
pub use persia_blob::{BlobId, BlobRef};
// Documents (SPEC §6.4): plain values (ADR-0064 item 2).
pub use persia_engine::document::{Document, Value};
/// How durable a write is before its call returns (SPEC §8.2.1): `Buffered`, `Async` or `Fsync` (the default).
pub use persia_engine::wal::Durability;
// Collection schemas (SPEC §6.2): plain values, re-exported as the engine has them (ADR-0064 item 2).
pub use persia_engine::schema::{
    AnalyzerRef, BuiltinAnalyzer, CollectionSchema, Fallback, Field, FieldId, FieldKind, FieldType,
    FilterRelaxation, KeywordOptions, Level, Levels, RankFn, RankingConfig, RelaxationConfig,
    SchemaBuilder, StaticRank, TextOptions,
};
