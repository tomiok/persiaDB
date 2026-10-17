//! Files Persia leaves beside its data: temporary files of writes a crash interrupted, and quarantine copies of a
//! corrupt container tail (ADR-0014). None of them holds acknowledged data, except possibly a quarantine copy, which
//! is for an operator to inspect.

use std::io;
use std::path::PathBuf;

use crate::container::ContainerFile;

/// A file found beside a container or in a local store, from [`LocalStorage::sweep_temporaries`](crate::LocalStorage::sweep_temporaries)
/// or [`Container::side_files`](crate::container::Container::side_files).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideFile {
    /// Where it is.
    pub path: PathBuf,
    /// What it is.
    pub kind: SideFileKind,
    /// Whether a live writer is using it.
    pub in_use: InUse,
    /// Whether the call that reported it removed it.
    pub removed: bool,
}

/// What a [`SideFile`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideFileKind {
    /// The temporary file of a write: a local store's `put` (`@tmp.*`, SPEC §10.1.1), or a container's create or
    /// compaction (`<name>.tmp.*`, `<name>.compact.*`, SPEC §4.5.1, §4.6). Left behind, it holds no acknowledged data.
    Temporary,
    /// The copy of a corrupt container tail a recovery made before truncating it (`<name>.quarantine.*`, ADR-0014).
    Quarantine,
}

/// Whether a live writer is using a [`SideFile`]: each one holds the file's lock while it does (on Unix).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InUse {
    /// No writer holds it: a temporary file is stale.
    No,
    /// A live writer holds its lock.
    Yes,
    /// Files cannot be locked here (Windows, SPEC §4.5.1), so whether a writer uses it is unknown.
    Unknown,
}

/// Takes `file`'s lock if it can: `Some(true)` with the lock held until `file` is dropped, `Some(false)` if another
/// handle holds it, `None` where files cannot be locked (Windows).
pub(crate) fn try_lock<F: ContainerFile>(file: &F) -> io::Result<Option<bool>> {
    match file.try_lock() {
        Ok(locked) => Ok(Some(locked)),
        Err(err) if err.kind() == io::ErrorKind::Unsupported => Ok(None),
        Err(err) => Err(err),
    }
}

impl InUse {
    /// The state [`try_lock`] found.
    pub(crate) fn of(locked: Option<bool>) -> Self {
        match locked {
            Some(true) => Self::No,
            Some(false) => Self::Yes,
            None => Self::Unknown,
        }
    }
}
