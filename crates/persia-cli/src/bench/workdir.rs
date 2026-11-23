//! The scratch directory of `persia bench` (ROADMAP 10.4.3): a new directory the run creates, fills and removes.
//!
//! The run never writes outside it, and it never exists before the run: it is made with `create_dir`, which fails on
//! any existing name (a file, a directory, a symbolic link), so nothing a user has there is opened, overwritten or
//! removed. Removing it afterwards removes only what this run put in it. The directory is removed when the run ends,
//! also when it fails; a run that is killed leaves it behind (`persia-bench-*`, safe to delete).

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Names tried before giving up: each differs only by its counter, so this many leftovers of one process id would
/// have to exist.
const ATTEMPTS: u32 = 1000;

/// A directory this run created, removed when the value is dropped.
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
    /// Whether [`WorkDir::remove`] already removed it.
    removed: bool,
}

impl WorkDir {
    /// Creates a new directory `persia-bench-<process id>-<n>` in `base`, which must be an existing directory.
    pub fn create_in(base: &Path) -> Result<Self> {
        if !base.is_dir() {
            bail!("{} is not a directory", base.display());
        }
        let pid = std::process::id();
        for n in 0..ATTEMPTS {
            let path = base.join(format!("persia-bench-{pid}-{n}"));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        removed: false,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        bail!(
            "{} already holds {ATTEMPTS} directories named persia-bench-{pid}-*; remove them",
            base.display()
        )
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Removes the directory and everything the run put in it, reporting a failure (dropping it only warns).
    pub fn remove(mut self) -> Result<()> {
        self.removed = true;
        std::fs::remove_dir_all(&self.path)
            .with_context(|| format!("removing {}", self.path.display()))
    }
}

impl Drop for WorkDir {
    /// The run failed, or is unwinding: the directory is still removed, and a failure to do so is reported, since a
    /// drop cannot return it.
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            eprintln!(
                "persia: could not remove the bench directory {}: {e}",
                self.path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_work_directory_is_new_and_removed_on_drop_with_its_files() {
        let base = tempfile::tempdir().unwrap();
        let work = WorkDir::create_in(base.path()).unwrap();
        let name = format!("persia-bench-{}-0", std::process::id());
        assert_eq!(work.path(), base.path().join(&name));
        assert_eq!(entries(base.path()), [name]);
        std::fs::write(work.path().join("bench.persia"), b"bytes").unwrap();
        std::fs::create_dir(work.path().join("sub")).unwrap();
        drop(work);
        assert_eq!(entries(base.path()), [] as [String; 0]);
    }

    #[test]
    fn remove_reports_and_leaves_nothing_for_the_drop() {
        let base = tempfile::tempdir().unwrap();
        let work = WorkDir::create_in(base.path()).unwrap();
        std::fs::write(work.path().join("bench.persia"), b"bytes").unwrap();
        work.remove().unwrap();
        assert_eq!(entries(base.path()), [] as [String; 0]);

        // Removed behind its back: `remove` says so.
        let work = WorkDir::create_in(base.path()).unwrap();
        std::fs::remove_dir(work.path()).unwrap();
        let err = work.remove().unwrap_err();
        assert!(format!("{err:#}").contains("removing "), "{err:#}");
    }

    /// An existing name is never reused, whatever it is: the next free one is taken, and what was there stays.
    #[test]
    fn an_existing_name_is_skipped_and_left_alone() {
        let base = tempfile::tempdir().unwrap();
        let pid = std::process::id();
        let taken = base.path().join(format!("persia-bench-{pid}-0"));
        std::fs::create_dir(&taken).unwrap();
        std::fs::write(taken.join("app.persia"), b"a user's database").unwrap();
        std::fs::write(base.path().join(format!("persia-bench-{pid}-1")), b"a file").unwrap();
        let work = WorkDir::create_in(base.path()).unwrap();
        assert_eq!(
            work.path(),
            base.path().join(format!("persia-bench-{pid}-2"))
        );
        drop(work);
        assert_eq!(
            std::fs::read(taken.join("app.persia")).unwrap(),
            b"a user's database"
        );
        assert_eq!(
            entries(base.path()),
            [
                format!("persia-bench-{pid}-0"),
                format!("persia-bench-{pid}-1")
            ]
        );
    }

    #[test]
    fn a_base_that_is_not_a_directory_is_refused_and_not_created() {
        let base = tempfile::tempdir().unwrap();
        let missing = base.path().join("missing");
        let err = WorkDir::create_in(&missing).unwrap_err();
        assert!(format!("{err:#}").contains("is not a directory"), "{err:#}");
        assert!(!missing.exists());
        let file = base.path().join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(WorkDir::create_in(&file).is_err());
    }
}
