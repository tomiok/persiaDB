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
    /// No writer holds it. A temporary file is then stale; a quarantine copy is not in use, but may hold acknowledged
    /// writes (ADR-0014), so `No` alone never means "safe to delete".
    No,
    /// A live writer holds its lock.
    Yes,
    /// Whether a writer uses it cannot be told: files cannot be locked here (Windows, SPEC §4.5.1), or a lock belongs to
    /// the process rather than the handle (a network or FUSE file system, ADR-0019).
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

/// Whether a writer uses `file`, found by taking its lock for a moment ([`try_lock`]). With [`InUse::No`] this handle
/// holds the lock: release it with [`unlock_kept`] if the file stays. A lock taken where locks belong to the process
/// ([`ContainerFile::locks_per_handle`]) proves nothing: [`InUse::Unknown`].
pub(crate) fn in_use<F: ContainerFile>(file: &F) -> io::Result<InUse> {
    let locked = try_lock(file)?;
    let per_handle = locked != Some(true) || file.locks_per_handle();
    Ok(InUse::of(locked, per_handle))
}

/// Releases the lock this handle took on a side file it keeps, then returns `result`, the caller's outcome (an unlock
/// failure is added to its error, or becomes the error).
///
/// Closing the file is not enough: `flock` belongs to the open file description, and a child that another thread
/// forked keeps a copy of the descriptor until its `exec` (SPEC §4.5.1). The lock would outlive the listing or sweep,
/// and the next one would find the stale file in use and keep it (ROADMAP 2.3.11). A removed file needs no unlock: its
/// name is gone, so no one can meet the lock. Call it only for a lock that is this handle's own ([`InUse::No`]): where
/// locks belong to the process (POSIX record locks behind `flock`, ADR-0019) a fork inherits none, and the close
/// releases them anyway, a lock this process's writer holds included.
pub(crate) fn unlock_kept<F: ContainerFile, T>(file: &F, result: io::Result<T>) -> io::Result<T> {
    match (result, file.unlock()) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(err)) | (Err(err), Ok(())) => Err(err),
        (Err(err), Err(unlock)) => Err(io::Error::new(
            err.kind(),
            format!("{err}; unlocking the file then failed: {unlock}"),
        )),
    }
}

impl InUse {
    /// The state [`try_lock`] found, given whether locks are per handle on the file's file system.
    pub(crate) fn of(locked: Option<bool>, per_handle: bool) -> Self {
        match locked {
            Some(true) if per_handle => Self::No,
            Some(false) => Self::Yes,
            Some(true) | None => Self::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::InUse;

    #[test]
    fn a_lock_proves_a_file_free_only_where_locks_are_per_handle() {
        assert_eq!(InUse::of(Some(true), true), InUse::No);
        // NFS, FUSE: the lock may be this process's own writer's.
        assert_eq!(InUse::of(Some(true), false), InUse::Unknown);
        assert_eq!(InUse::of(Some(false), true), InUse::Yes);
        assert_eq!(InUse::of(Some(false), false), InUse::Yes);
        // Windows: no lock at all.
        assert_eq!(InUse::of(None, true), InUse::Unknown);
    }
}
