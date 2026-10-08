//! On-disk primitives for Persia DB: encodings, frames, superblock, checksums (SPEC §4, §5). No I/O policy.
//!
//! The only crate allowed to contain `unsafe` (in `mmap`, under the ADR from ROADMAP 1.8.3); everywhere else it stays denied.
