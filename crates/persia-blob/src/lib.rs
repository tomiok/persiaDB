//! Content-addressed blob store for Persia DB: chunks, dedup, GC (SPEC §9).
//!
//! A blob is identified by the SHA-256 of its bytes ([`BlobId`], ADR-0002), computed by [`ContentHasher`] while the
//! bytes are cut into fixed-size chunks ([`ChunkSize`], SPEC §9.2, ADR-0034). The value types an engine stores
//! ([`BlobId`], [`persia_format::BlobRef`]) live in `persia-format`.
#![forbid(unsafe_code)]
// Sizes and offsets come from disk and the network: arithmetic must be checked (ROADMAP 1.1.5).
#![deny(clippy::arithmetic_side_effects)]
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]
// Sizes convert with `From`, `persia_format::widen` or `try_from`, never a silent `as` (CLAUDE.md checked ops).
#![cfg_attr(not(test), deny(clippy::as_conversions))]

pub mod chunker;
pub mod chunking;
pub mod error;
pub mod hash;
pub mod manifest;

pub use chunker::{Chunk, ChunkedBlob, Chunker, chunk_all};
pub use chunking::ChunkSize;
pub use error::{Error, Result};
pub use hash::{ContentHasher, blob_id_of};
pub use manifest::{BlobManifest, UploadId};
pub use persia_format::BlobId;
