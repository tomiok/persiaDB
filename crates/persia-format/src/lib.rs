//! On-disk primitives for Persia DB: encodings, frames, superblock, checksums (SPEC §4, §5). No I/O policy.
//!
//! The only crate allowed to contain `unsafe` (in `mmap`, under the ADR from ROADMAP 1.8.3); everywhere else it stays denied.

// Offsets and lengths come from disk: arithmetic must be checked or explicitly wrapping (ROADMAP 1.1.5).
#![deny(clippy::arithmetic_side_effects)]
// Unit tests compute expected values with plain operators; the library build above is still checked.
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

pub mod align;
pub mod error;
pub mod offset;
pub mod reader;
pub mod varint;
pub mod writer;
pub mod zigzag;

pub use align::ALIGNMENT;
pub use error::{Corruption, Error, Result};
pub use offset::{Len, Offset};
pub use reader::Reader;
pub use writer::Writer;
