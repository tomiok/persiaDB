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

    use std::path::PathBuf;

    use persia_storage::container::sim::CrashLoss;
    use persia_storage::vfs::sim::SimFs;
    use persia_storage::vfs::{DirEntry, EntryKind};

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

    /// `fs` with every directory sync a no-op that reports success: the walk of [`create_parent_dirs_in`], unchanged,
    /// without its syncs.
    #[derive(Debug)]
    struct SkipsDirSyncs(SimFs);

    impl Vfs for SkipsDirSyncs {
        type File = <SimFs as Vfs>::File;

        fn open(&self, path: &Path, write: bool) -> io::Result<Self::File> {
            self.0.open(path, write)
        }
        fn create_new(&self, path: &Path) -> io::Result<Self::File> {
            self.0.create_new(path)
        }
        fn create_dir(&self, path: &Path) -> io::Result<()> {
            self.0.create_dir(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.0.rename(from, to)
        }
        fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.0.hard_link(from, to)
        }
        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.0.remove_file(path)
        }
        fn sync_dir(&self, _: &Path) -> io::Result<()> {
            Ok(())
        }
        fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
            self.0.entry_kind(path)
        }
        fn is_dir(&self, path: &Path) -> io::Result<bool> {
            self.0.is_dir(path)
        }
        fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
            self.0.read_dir(dir)
        }
        fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
            self.0.canonicalize(path)
        }
        fn same_file(&self, file: &Self::File, path: &Path) -> io::Result<bool> {
            self.0.same_file(file, path)
        }
        fn lock_beside(&self, path: &Path) -> io::Result<Option<Self::File>> {
            self.0.lock_beside(path)
        }
    }

    /// The control of `new_parents_survive_a_crash`: the same scenario through the same walk with its directory syncs
    /// skipped loses every new directory in the crash, so the crash does find a missing sync.
    /// A bare file name has no parent directory to create: nothing is asked of the file system (the empty parent
    /// cannot even be made absolute).
    #[test]
    fn a_bare_file_name_needs_no_directory() {
        let fs = SimFs::new();
        let before = fs.mutations();
        create_parent_dirs_in(&fs, Path::new("app.persia")).unwrap();
        assert_eq!(fs.mutations(), before);
    }

    /// The operating system's file system: the directories are created.
    #[test]
    fn on_the_real_file_system_the_parents_are_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x").join("y").join("app.persia");
        create_parent_dirs(&path).unwrap();
        assert!(dir.path().join("x").join("y").is_dir());
        assert!(!path.exists());
    }

    #[test]
    fn without_the_syncs_the_same_crash_loses_them() {
        let fs = SimFs::new();
        fs.create_dir(Path::new("/data")).unwrap();
        fs.sync_dir(Path::new("/")).unwrap();
        let skipping = SkipsDirSyncs(fs);
        create_parent_dirs_in(&skipping, Path::new("/data/a/b/c/app.persia")).unwrap();
        // Before the crash they exist; the crash takes every one the walk created.
        for dir in ["/data/a", "/data/a/b", "/data/a/b/c"] {
            assert!(skipping.0.is_dir(Path::new(dir)).unwrap(), "{dir}");
        }
        let crashed = skipping.0.crash(CrashLoss::All);
        assert!(crashed.is_dir(Path::new("/data")).unwrap());
        for dir in ["/data/a", "/data/a/b", "/data/a/b/c"] {
            assert!(crashed.is_dir(Path::new(dir)).is_err(), "{dir}");
        }
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
