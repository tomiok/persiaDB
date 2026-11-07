//! A listing or sweep that took a temporary file's lock releases it before closing the file (ROADMAP 2.3.11).
//!
//! `flock` belongs to the open file description, so closing a descriptor does not release its lock while a duplicate
//! is open: a child that another thread of this process forked keeps a copy of every descriptor until its `exec`
//! (SPEC §4.5.1). A listing that only closed its handle left the lock with that child, and the sweep that followed
//! found the stale file "in use" and kept it (`a_temporary_a_live_process_holds_is_kept_until_the_process_is_gone`
//! failed this way once under `just check`).
//!
//! [`ForkedFs`] makes that window deterministic: it is the real file system, and it keeps a duplicate of every
//! descriptor it opens, as a forked child between `fork` and `exec` does.

#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use persia_storage::container::{Container, ContainerFile, Mode};
use persia_storage::vfs::{DirEntry, EntryKind, RealFs, Vfs};
use persia_storage::{InUse, LocalStorage};

/// The real file system, keeping a duplicate of every file it opens or creates: the copies a child of this process
/// holds between `fork` and `exec`. `fail_removes` makes `remove_file` fail, leaving the file in place.
#[derive(Debug, Clone, Default)]
struct ForkedFs {
    copies: Arc<Mutex<Vec<File>>>,
    fail_removes: Arc<AtomicBool>,
}

impl ForkedFs {
    fn keep(&self, file: io::Result<File>) -> io::Result<File> {
        let file = file?;
        self.copies.lock().unwrap().push(file.try_clone()?);
        Ok(file)
    }

    fn copies(&self) -> usize {
        self.copies.lock().unwrap().len()
    }
}

impl Vfs for ForkedFs {
    type File = File;

    fn open(&self, path: &Path, write: bool) -> io::Result<File> {
        self.keep(RealFs.open(path, write))
    }

    fn create_new(&self, path: &Path) -> io::Result<File> {
        self.keep(RealFs.create_new(path))
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        RealFs.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.hard_link(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if self.fail_removes.load(Ordering::Relaxed) {
            return Err(io::Error::other("injected: remove failed"));
        }
        RealFs.remove_file(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        RealFs.sync_dir(dir)
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        RealFs.entry_kind(path)
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        RealFs.is_dir(path)
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        RealFs.read_dir(dir)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        RealFs.canonicalize(path)
    }

    fn same_file(&self, file: &File, path: &Path) -> io::Result<bool> {
        RealFs.same_file(file, path)
    }

    fn lock_beside(&self, path: &Path) -> io::Result<Option<File>> {
        self.keep_beside(RealFs.lock_beside(path))
    }
}

impl ForkedFs {
    fn keep_beside(&self, file: io::Result<Option<File>>) -> io::Result<Option<File>> {
        file?.map(|f| self.keep(Ok(f))).transpose()
    }
}

/// A temporary directory whose file system has per-handle locks, or `None` (after saying so) where it has not:
/// overlayfs (a container's `/tmp`), NFS and FUSE report every temporary file `Unknown` (ADR-0019 decision 6), which
/// these tests do not cover. CI's temporary directory is a local disk; `sys::tests::a_temporary_file_is_on_a_local_disk`
/// fails if it is not, so this skip cannot hide the suite there.
fn local_tempdir() -> Option<tempfile::TempDir> {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("probe");
    std::fs::write(&probe, b"").unwrap();
    if RealFs.open(&probe, false).unwrap().locks_per_handle() {
        std::fs::remove_file(&probe).unwrap();
        Some(dir)
    } else {
        eprintln!(
            "skipped: locks are not per handle on the file system of {}",
            dir.path().display()
        );
        None
    }
}

fn report(files: &[persia_storage::SideFile]) -> Vec<(PathBuf, InUse, bool)> {
    files
        .iter()
        .map(|f| (f.path.clone(), f.in_use, f.removed))
        .collect()
}

/// The premise: a duplicate keeps a lock after the handle that took it is closed, so a close alone is not a release.
/// Without it the tests below would pass on code that leaks the lock.
#[test]
fn a_duplicate_descriptor_keeps_the_lock_after_close() {
    let Some(dir) = local_tempdir() else { return };
    let path = dir.path().join("f");
    std::fs::write(&path, b"x").unwrap();
    let fs = ForkedFs::default();
    let file = fs.open(&path, false).unwrap();
    assert!(ContainerFile::try_lock(&file).unwrap());
    drop(file);
    assert_eq!(fs.copies(), 1);
    assert!(
        !ContainerFile::try_lock(&RealFs.open(&path, false).unwrap()).unwrap(),
        "the duplicate must keep the lock, or the tests here prove nothing"
    );
}

/// A listing that found a stale temporary file free leaves it free: the sweep after it removes it, while every
/// descriptor either opened is still duplicated.
#[test]
fn a_sweep_after_a_listing_removes_a_stale_temporary_despite_forked_copies() {
    let Some(dir) = local_tempdir() else { return };
    let root = dir.path().join("store");
    let fs = ForkedFs::default();
    let store = LocalStorage::open_in(fs.clone(), &root).unwrap();
    let stale = root.join("@tmp.0123456789abcdef.0");
    std::fs::write(&stale, b"a crashed put").unwrap();

    let listed = store.temporaries().unwrap();
    assert_eq!(report(&listed), [(stale.clone(), InUse::No, false)]);
    assert!(
        fs.copies() > 0,
        "the listing's descriptor was not duplicated"
    );

    let listed = store.temporaries().unwrap();
    assert_eq!(
        report(&listed),
        [(stale.clone(), InUse::No, false)],
        "a second listing"
    );
    let swept = store.sweep_temporaries(false).unwrap();
    assert_eq!(report(&swept), [(stale.clone(), InUse::No, true)]);
    assert!(!stale.exists());
}

/// A sweep whose removal fails keeps the file and returns the error; it leaves the file free, so the next sweep
/// removes it.
#[test]
fn a_sweep_whose_removal_failed_leaves_the_temporary_free() {
    let Some(dir) = local_tempdir() else { return };
    let root = dir.path().join("store");
    let fs = ForkedFs::default();
    let store = LocalStorage::open_in(fs.clone(), &root).unwrap();
    let stale = root.join("@tmp.0123456789abcdef.0");
    std::fs::write(&stale, b"a crashed put").unwrap();

    fs.fail_removes.store(true, Ordering::Relaxed);
    let err = store.sweep_temporaries(false).unwrap_err();
    assert!(err.to_string().contains("injected"), "{err}");
    assert!(stale.exists());
    fs.fail_removes.store(false, Ordering::Relaxed);

    let swept = store.sweep_temporaries(false).unwrap();
    assert_eq!(report(&swept), [(stale.clone(), InUse::No, true)]);
}

/// The same for a container's side files: `side_files_in` lists a stale `<name>.tmp.*` free and leaves it free, and a
/// `sweep_stale_files` whose removal failed leaves it free too, so the next one removes it.
#[test]
fn container_side_files_stay_free_after_a_listing_or_a_failed_sweep() {
    let Some(dir) = local_tempdir() else { return };
    // Canonical, as the container reports it (macOS `/var` is a symbolic link to `/private/var`).
    let dir_path = std::fs::canonicalize(dir.path()).unwrap();
    let path = dir_path.join("app.persia");
    drop(Container::create(&path).unwrap());
    let stale = dir_path.join("app.persia.tmp.0123456789abcdef.3");
    std::fs::write(&stale, b"half a container").unwrap();
    let fs = ForkedFs::default();

    let files = Container::side_files_in(&fs, &path).unwrap();
    assert_eq!(report(&files), [(stale.clone(), InUse::No, false)]);
    assert!(
        fs.copies() > 0,
        "the listing's descriptor was not duplicated"
    );

    let c = Container::open_in(Arc::new(fs.clone()), &path, Mode::ReadWrite).unwrap();
    fs.fail_removes.store(true, Ordering::Relaxed);
    let err = c.sweep_stale_files().unwrap_err();
    assert!(err.to_string().contains("injected"), "{err}");
    assert!(stale.exists());
    fs.fail_removes.store(false, Ordering::Relaxed);

    assert_eq!(c.sweep_stale_files().unwrap(), std::slice::from_ref(&stale));
    assert!(!stale.exists());
}
