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
pub mod store;

pub use chunker::{Chunk, ChunkedBlob, Chunker, chunk_all};
pub use chunking::ChunkSize;
pub use error::{Error, Result};
pub use hash::{ContentHasher, blob_id_of};
pub use manifest::{BlobManifest, UploadId};
pub use persia_format::BlobId;
#[cfg(any(test, feature = "testing"))]
pub use store::MemoryBlobStore;
pub use store::{
    BindOutcome, BlobStore, SMALL_BLOB_MAX, read_chunk_checked, read_manifest_range, read_range,
};

// -- pack (9.2) --
// The local pack file (SPEC §9.3): its `mod` and `pub use` lines go here, and only here.
pub mod pack;
pub use pack::{PackMode, PackStore, WriterProof, pack_path};

// -- remote (9.3) --
// The remote layout (SPEC §9.4): its `mod` and `pub use` lines go here, and only here.
pub mod remote;
pub use remote::RemoteBlobStore;
//
//

// -- documents (9.4) --
// What the blobs-with-documents batch needs from this crate: `put_blob` and `get_blob_range` themselves live in
// `persia::internal::blob` (ADR-0051), so only this re-export is here.
// The value type `put_blob` returns (it lives in `persia-format`, which `persia` reaches only through this crate).
pub use persia_format::BlobRef;

// -- gc (9.5) --
// Garbage collection (SPEC §9.5; ROADMAP 9.5.1, 9.5.2, 9.5.8): its `mod` and `pub use` lines go here, and only here.
pub mod gc;
pub use gc::{GcObject, Mark, PinCheck, PinGate, Rewrite, Sweep, SweepOptions, SweepReport};

// -- followups (9.2, 9.3, 9.4) --
// Wave 2's pack and put_blob follow-ups (ROADMAP 9.2.6, 9.2.8, 9.2.9, 9.4.7): their `mod` and `pub use` lines, if any.
// `read_manifest_range` with the caller's buffers, for streams (ROADMAP 9.2.9).
pub use store::read_manifest_range_into;
//

// -- wiring (9.2.7, 9.4.5, 9.5.3, 9.5.6) --
// Wave 3's pack wiring and verify (blob/wiring): its `mod` and `pub use` lines, if any, go here, and only here.
//
//

// -- resume (9.3.4, 9.3.6, 9.5.9) --
// Wave 3's resumed uploads (blob/resume): its `mod` and `pub use` lines, if any, go here, and only here.
pub use remote::ResumeToken;
//
