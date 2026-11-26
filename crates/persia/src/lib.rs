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
//! that returned; commits and merges run on the database's own thread, and [`Db::close`] joins it (ADR-0064). Every
//! call returns [`Result`], whose [`Error`] carries a stable [`ErrorCode`] (ADR-0063). With the cargo feature
//! `async`, `AsyncDb` and `AsyncCollection` return futures that need no runtime (ADR-0064). The engine
//! (`persia-engine`) and the hidden `internal` layer under this crate (ADR-0051) are not stable APIs.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// Sizes and offsets convert with `From` or `try_from`, never a silent `as` (CLAUDE.md checked ops).
#![cfg_attr(not(test), deny(clippy::as_conversions))]

// The async API (ADR-0064 items 10 to 15): the feature `async`, which adds no dependency.
#[cfg(feature = "async")]
mod asynchronous;
// `Db::backup` (ROADMAP 10.1.8, ADR-0067).
mod backup;
mod blob;
mod collection;
mod db;
mod documents;
mod error;
// The hidden layer under the facade (ADR-0051): public only with the `internal` feature, for `persia-cli` and the
// fuzz targets. Not a stable API.
#[cfg(feature = "internal")]
#[doc(hidden)]
pub mod internal;
#[cfg(not(feature = "internal"))]
#[allow(dead_code, unreachable_pub)]
// Items only `persia-cli` uses (GC, verify) are built but not reached.
mod internal;
mod location;
mod maintenance;
mod options;
mod search;
mod typed;

#[cfg(feature = "async")]
pub use asynchronous::{AsyncCollection, AsyncDb, JobFuture};
#[cfg(all(feature = "async", feature = "test-hooks"))]
#[doc(hidden)]
pub use asynchronous::{JobEvent, JobHook, JobKind};
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub use backup::{BackupHook, BackupStep};
pub use blob::{Blob, BlobReader, PutBlobOptions, ResumeToken};
pub use collection::Collection;
pub use db::{BlockCacheStats, Db};
pub use error::{Error, ErrorCode, Result};
#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub use options::DetachedSlot;
pub use options::OpenOptions;
// Search requests and responses (SPEC §7): plain values (ADR-0063 item 2). Every re-export of a lower crate is
// `#[doc(inline)]`: its items are documented here, as part of this crate, and its public-API snapshot lists them in
// full (ROADMAP 10.1.12, `crates/persia/public-api.txt`).
#[doc(inline)]
pub use persia_engine::query::{Filter, FilterValue};
#[doc(inline)]
pub use persia_engine::relax::{RelaxLevel, RelaxOptions};
#[doc(inline)]
pub use persia_engine::search::{
    Explanation, FacetCounts, FacetRequest, FacetValue, FieldExplanation, Missing, Order,
    PairExplanation, PhraseExplanation, ProximityExplanation, RelaxedExplanation, Sort, SortKey,
    WordExplanation,
};
pub use search::{Hit, Relaxation, SearchRequest, SearchResponse};
// Blob references (SPEC §9.1): plain values (ADR-0063 item 2).
#[doc(inline)]
pub use persia_blob::{BlobId, BlobRef};
// Documents (SPEC §6.4): plain values (ADR-0063 item 2).
#[doc(inline)]
pub use persia_engine::document::{Document, Value};
/// How durable a write is before its call returns (SPEC §8.2.1): `Buffered`, `Async` or `Fsync` (the default).
#[doc(inline)]
pub use persia_engine::wal::Durability;
// Collection schemas (SPEC §6.2): plain values, re-exported as the engine has them (ADR-0063 item 2).
#[doc(inline)]
pub use persia_engine::schema::{
    AnalyzerRef, BuiltinAnalyzer, CollectionSchema, Fallback, Field, FieldId, FieldKind, FieldType,
    FilterRelaxation, KeywordOptions, Level, Levels, RankFn, RankingConfig, RelaxationConfig,
    SchemaBuilder, StaticRank, TextOptions,
};
