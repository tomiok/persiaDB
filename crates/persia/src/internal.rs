//! The internal layer under the public embedded API (ADR-0051): blobs with their pin set, and documents that
//! reference them. **Not a stable API**: M10's `Db` and `Collection` wrap it (SPEC §9.6), and it may change without
//! notice until then.
//!
//! - [`blob`]: `put_blob`, `get_blob`, `get_blob_range` over a `BlobStore` (SPEC §9.2, §9.6; ROADMAP 9.4.2);
//! - [`pins`]: the writer's pin set GC honors (SPEC §9.5);
//! - [`retry`]: the clock and backoff of `put_blob`'s bind retries (SPEC §9.4);
//! - [`documents`]: `put(doc, blob)`, the blob durable before the document is logged (SPEC §9.5; ROADMAP 9.4.1).

pub mod blob;
pub mod documents;
pub mod pins;
pub mod retry;
