//! Storage trait and backends (memory, local file, object stores) for Persia DB (SPEC §10).
//!
//! Objects are named by validated [`Key`]s; every failure is an [`Error`] with one [`ErrorKind`].
// `deny`, not `forbid`: the container's mapped-read module must be able to `allow` it (ADR-0010, ADR-0017).
#![deny(unsafe_code)]
// Offsets and lengths come from disk and the network: arithmetic must be checked (ROADMAP 1.1.5).
#![deny(clippy::arithmetic_side_effects)]
#![cfg_attr(test, allow(clippy::arithmetic_side_effects))]

pub mod container;
pub mod error;
pub mod faulty;
pub mod key;
pub mod local;
pub mod memory;
pub mod meta;
pub mod retry;
pub mod storage;
pub mod vfs;

pub use error::{BoxError, Error, ErrorKind, Op, Result};
pub use key::{InvalidKey, Key, KeyDefect, KeyPrefix};
pub use local::LocalStorage;
pub use memory::MemoryStorage;
pub use meta::{Capabilities, ObjectMeta, PutOutcome};
pub use storage::{Storage, check_range, range_source};
