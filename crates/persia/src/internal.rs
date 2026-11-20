//! The internal layer under the public embedded API (ADR-0051): blobs with their pin set, and documents that
//! reference them. **Not a stable API**: M10's `Db` and `Collection` wrap it (SPEC §9.6), and it may change without
//! notice until then.
//!
//! - [`blob`]: `put_blob`, `get_blob`, `get_blob_range` over a `BlobStore` (SPEC §9.2, §9.6; ROADMAP 9.4.2);
//! - [`pins`]: the writer's pin set GC honors (SPEC §9.5);
//! - [`retry`]: the clock and backoff of `put_blob`'s bind retries (SPEC §9.4);
//! - [`documents`]: `put(doc, blob)`, the blob durable before the document is logged (SPEC §9.5; ROADMAP 9.4.1);
//! - [`gc`]: the mark and the sweep of the blob store (SPEC §9.5; ROADMAP 9.5.1, 9.5.2).

pub mod blob;
pub mod documents;
pub mod pins;
pub mod retry;

// -- gc (9.5) --
// Garbage collection (SPEC §9.5; ROADMAP 9.5.1, 9.5.2): `BlobDatabase::gc` and its options live in `gc`.
pub mod gc;

// -- wiring (9.4.5, 9.5.3, 9.5.6) --
// blob/wiring: the blob checks `persia verify` runs (ADR-0053: the CLI reaches them through `persia`) go here.
// `verify`: dangling references and the pack's checksum scan (SPEC §9.5; ROADMAP 9.5.3, 9.5.6).
pub mod verify;
// -- end wiring --

// -- resume (9.3.4, 9.3.6, 9.5.9) --
// blob/resume: its `mod` lines, if any, go here.
//
// -- end resume --
