//! The local blob pack file, `<name>.persia.blobs` beside `<name>.persia` (SPEC §9.3, ADR-0049).
//!
//! [`layout`] holds the pack's bytes: a header with the container header's layout and its own magic, two
//! double-buffered superblock slots that carry a pack generation, and append-only `BlobChunk` and `BlobIndex` frames.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub mod layout;
mod store;

pub use store::{PackMode, PackRecovery, PackStore};
// The writer-lock proof a read-write open takes (ROADMAP 9.2.7, ADR-0053), for callers that reach `persia-storage`
// only through this crate.
pub use persia_storage::container::WriterProof;

/// What the pack's file name adds to its container's: `app.persia` keeps its blobs in `app.persia.blobs` (SPEC §4).
pub const PACK_SUFFIX: &str = ".blobs";

/// The pack path of the container at `container`: the same name with [`PACK_SUFFIX`] appended.
///
/// ```
/// use std::path::Path;
/// assert_eq!(persia_blob::pack_path(Path::new("/data/app.persia")), Path::new("/data/app.persia.blobs"));
/// ```
pub fn pack_path(container: &Path) -> PathBuf {
    let mut name = OsString::from(container.as_os_str());
    name.push(PACK_SUFFIX);
    PathBuf::from(name)
}

// -- gc (9.5) --
// What the pack rewrite (`store/rewrite.rs`, ROADMAP 9.5.2) exports: its `pub use` lines go here, and only here.
//
//
