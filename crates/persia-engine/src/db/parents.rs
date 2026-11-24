//! Durable creation of a database file's missing parent directories (SPEC §4.5.2, ROADMAP 10.1.9): the facade's
//! `Db::open(path)` creates them before it creates the container, and a new directory's name is durable only once its
//! own parent is synced. `persia` reaches the file system only through the engine, so the walk of
//! [`persia_storage::vfs::create_dir_all_synced`] is offered here.

use std::io;
use std::path::Path;

use persia_storage::vfs::{RealFs, Vfs, create_dir_all_synced};

/// Creates the missing parent directories of the database file `path` on the operating system's file system, durably:
/// from the outermost missing one inward, each new directory's parent synced after it (SPEC §4.5.2). Existing
/// directories are left as they are. A path with no parent directory (a bare file name) needs none.
///
/// ```
/// use persia_engine::db::{Database, DatabaseOptions, create_parent_dirs};
///
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("data").join("2026").join("app.persia");
/// create_parent_dirs(&path)?;
/// assert!(path.parent().ok_or("no parent")?.is_dir());
/// // Done again, it changes nothing.
/// create_parent_dirs(&path)?;
/// let db = Database::create(&path, DatabaseOptions::default())?;
/// # drop(db);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// The OS error if a directory cannot be created or synced; [`io::ErrorKind::InvalidInput`] if an ancestor of
/// `path` exists and is not a directory.
pub fn create_parent_dirs(path: impl AsRef<Path>) -> io::Result<()> {
    create_parent_dirs_in(&RealFs, path.as_ref())
}

/// [`create_parent_dirs`] on `vfs` (a simulated file system in crash tests).
///
/// # Errors
/// As [`create_parent_dirs`].
pub fn create_parent_dirs_in<V: Vfs + ?Sized>(vfs: &V, path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            create_dir_all_synced(vfs, parent)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // Tests: inputs are fixed.

    use persia_storage::container::sim::CrashLoss;
    use persia_storage::vfs::sim::SimFs;

    use super::*;

    /// Every new directory survives a crash that drops whatever was not synced: each one's parent was synced.
    #[test]
    fn new_parents_survive_a_crash() {
        let fs = SimFs::new();
        fs.create_dir(Path::new("/data")).unwrap();
        fs.sync_dir(Path::new("/")).unwrap();
        create_parent_dirs_in(&fs, Path::new("/data/a/b/c/app.persia")).unwrap();
        let crashed = fs.crash(CrashLoss::All);
        for dir in ["/data", "/data/a", "/data/a/b", "/data/a/b/c"] {
            assert!(crashed.is_dir(Path::new(dir)).unwrap(), "{dir}");
        }
        // Done again on the crashed file system, it changes nothing.
        create_parent_dirs_in(&crashed, Path::new("/data/a/b/c/app.persia")).unwrap();
    }

    /// Without the syncs the same crash loses them: the test above checks what it claims to.
    #[test]
    fn unsynced_directories_are_lost_by_the_crash() {
        let fs = SimFs::new();
        fs.create_dir(Path::new("/data")).unwrap();
        let crashed = fs.crash(CrashLoss::All);
        assert!(crashed.is_dir(Path::new("/data")).is_err());
    }

    /// A file where a directory should be is an error, not a silent success.
    #[test]
    fn a_file_in_the_way_is_an_error() {
        let fs = SimFs::new();
        drop(fs.create_new(Path::new("/data")).unwrap());
        let err = create_parent_dirs_in(&fs, Path::new("/data/app.persia")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
