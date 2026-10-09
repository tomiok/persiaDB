//! On-disk primitives for Persia DB: encodings, frames, superblock, checksums (SPEC §4, §5). No I/O policy.
//!
//! The only crate allowed to contain `unsafe` (in `mmap`, under the ADR from ROADMAP 1.8.3); everywhere else it stays denied.

pub mod align;
pub mod error;
pub mod reader;
pub mod writer;

pub use align::ALIGNMENT;
pub use error::{Corruption, Error, Result};
pub use reader::Reader;
pub use writer::Writer;
