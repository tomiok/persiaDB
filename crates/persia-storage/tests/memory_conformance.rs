//! `MemoryStorage` passes the `Storage` conformance suite (ROADMAP 2.2.1, 2.2.3).

#[macro_use]
mod conformance;

use persia_storage::MemoryStorage;

storage_conformance!(MemoryStorage::new());
