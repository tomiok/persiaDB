//! Persia DB: an embedded, single-file, cloud-native search database.
//!
//! This crate is the public embedded API (ROADMAP M10; SPEC §6.2, §7, §8.1, §9.6, §14). It is being built leaf by
//! leaf: so far it has the error type every call returns, [`Error`], with its stable [`ErrorCode`]s (ROADMAP
//! 10.1.5, ADR-0064). The write path and search are library code in `persia-engine`, and blobs with documents are
//! the hidden [`internal`] layer (ADR-0051), neither of which is a stable API.
#![forbid(unsafe_code)]
#![deny(missing_docs)]
// Sizes and offsets convert with `From` or `try_from`, never a silent `as` (CLAUDE.md checked ops).
#![cfg_attr(not(test), deny(clippy::as_conversions))]

mod error;
#[doc(hidden)]
pub mod internal;

pub use error::{Error, ErrorCode, Result};
