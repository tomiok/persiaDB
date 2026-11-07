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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use persia_storage::container::{Container, ContainerFile, Mode};
use persia_storage::vfs::{DirEntry, EntryKind, RealFs, Vfs};
use persia_storage::{Error, InUse, Key, LocalStorage, Storage};

/// The real file system, keeping a duplicate of every file it opens or creates: the copies a child of this process
/// holds between `fork` and `exec`. The `fail_*` flags make that call fail without effect (`fail_removes` after
/// `passing_removes` more removals succeed); `read_only_creates` hands `create_new`'s caller a read-only handle, so its
/// writes fail.
#[derive(Debug, Clone, Default)]
struct ForkedFs {
    copies: Arc<Mutex<Vec<File>>>,
    fail_removes: Arc<AtomicBool>,
    passing_removes: Arc<AtomicUsize>,
    fail_renames: Arc<AtomicBool>,
    fail_links: Arc<AtomicBool>,
    fail_same_file: Arc<AtomicBool>,
    fail_syncs: Arc<AtomicBool>,
    read_only_creates: Arc<AtomicBool>,
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

    fn injected(flag: &AtomicBool, what: &str) -> io::Result<()> {
        if flag.load(Ordering::Relaxed) {
            return Err(io::Error::other(format!("injected: {what} failed")));
        }
        Ok(())
    }
}

impl Vfs for ForkedFs {
    type File = File;

    fn open(&self, path: &Path, write: bool) -> io::Result<File> {
        self.keep(RealFs.open(path, write))
    }

    fn create_new(&self, path: &Path) -> io::Result<File> {
        let file = RealFs.create_new(path)?;
        if self.read_only_creates.load(Ordering::Relaxed) {
            drop(file);
            return self.keep(RealFs.open(path, false));
        }
        self.keep(Ok(file))
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        RealFs.create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        Self::injected(&self.fail_renames, "rename")?;
        RealFs.rename(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        // A kind of its own, so a test sees whether the error reported keeps it.
        if self.fail_links.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected: link failed",
            ));
        }
        RealFs.hard_link(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let passes = self
            .passing_removes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok();
        if !passes {
            Self::injected(&self.fail_removes, "remove")?;
        }
        RealFs.remove_file(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        Self::injected(&self.fail_syncs, "sync")?;
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
        Self::injected(&self.fail_same_file, "identity check")?;
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

/// Every message in `err`'s source chain, outermost first.
fn chain(err: &dyn std::error::Error) -> Vec<String> {
    let mut out = vec![err.to_string()];
    let mut source = err.source();
    while let Some(err) = source {
        out.push(err.to_string());
        source = err.source();
    }
    out
}

/// Which step of a write fails, before its cleanup removal fails too (ROADMAP 2.3.12).
#[derive(Debug, Clone, Copy)]
enum FailedStep {
    /// `put`: the rename over the object.
    Rename,
    /// `put_if_absent`: the link to the object name.
    Link,
    /// `put_if_absent`: none; the link succeeded, only the removal of the temporary name fails.
    NoneButTheRemoval,
    /// `put` (`write_temp`): the check that the locked file still has its name.
    IdentityCheck,
    /// `put` (`write_temp`): writing the data.
    Write,
}

/// A `put` or `put_if_absent` that fails and cannot remove its temporary file leaves it free, not locked by a forked
/// copy of its descriptor: a listing then reports it stale, and a sweep removes it (ROADMAP 2.3.12, ADR-0019 decision
/// 7). The step's own error stays the source of the one reported.
#[test]
fn a_write_that_failed_leaving_its_temporary_leaves_it_free() {
    let Some(dir) = local_tempdir() else { return };
    for (i, step) in [
        FailedStep::Rename,
        FailedStep::Link,
        FailedStep::NoneButTheRemoval,
        FailedStep::IdentityCheck,
        FailedStep::Write,
    ]
    .into_iter()
    .enumerate()
    {
        let root = dir.path().join(format!("store{i}"));
        let fs = ForkedFs::default();
        let store = LocalStorage::open_in(fs.clone(), &root).unwrap();
        let key = Key::new("k").unwrap();
        let data = Bytes::from_static(b"v");
        fs.fail_removes.store(true, Ordering::Relaxed);
        // A write to a read-only descriptor is `EBADF` (9 on Linux and macOS).
        let bad_fd = io::Error::from_raw_os_error(9).to_string();
        let (err, cause) = match step {
            FailedStep::Rename => {
                fs.fail_renames.store(true, Ordering::Relaxed);
                (
                    store.put(&key, data).unwrap_err(),
                    "injected: rename failed",
                )
            }
            FailedStep::Link => {
                fs.fail_links.store(true, Ordering::Relaxed);
                let err = store.put_if_absent(&key, data).unwrap_err();
                (err, "injected: link failed")
            }
            FailedStep::NoneButTheRemoval => {
                let err = store.put_if_absent(&key, data).unwrap_err();
                (err, "injected: remove failed")
            }
            FailedStep::IdentityCheck => {
                fs.fail_same_file.store(true, Ordering::Relaxed);
                let err = store.put(&key, data).unwrap_err();
                (err, "injected: identity check failed")
            }
            FailedStep::Write => {
                fs.read_only_creates.store(true, Ordering::Relaxed);
                (store.put(&key, data).unwrap_err(), &*bad_fd)
            }
        };
        for flag in [
            &fs.fail_removes,
            &fs.fail_renames,
            &fs.fail_links,
            &fs.fail_same_file,
            &fs.read_only_creates,
        ] {
            flag.store(false, Ordering::Relaxed);
        }

        let listed = store.temporaries().unwrap();
        assert_eq!(listed.len(), 1, "{step:?}: {listed:?}");
        assert_eq!(
            listed[0].in_use,
            InUse::No,
            "{step:?}: the failed write's lock outlived it in a forked copy"
        );
        let messages = chain(&err);
        assert!(
            messages.iter().any(|m| m == cause),
            "{step:?}: the step's error is not a source: {messages:?}"
        );
        assert!(
            messages.join(" | ").contains("injected: remove failed"),
            "{step:?}: the removal's error is lost: {messages:?}"
        );
        // The step's own `io::Error`: the source of the combined one, or the reported one itself when only the
        // removal failed.
        let Error::Io {
            source: reported, ..
        } = &err
        else {
            panic!("{step:?}: not an I/O error: {err:?}")
        };
        let own = std::error::Error::source(reported)
            .and_then(|s| s.downcast_ref::<io::Error>())
            .unwrap_or(reported);
        assert_eq!(own.to_string(), cause, "{step:?}");
        assert_eq!(reported.kind(), own.kind(), "{step:?}: the kind is lost");
        match step {
            FailedStep::Link => assert_eq!(own.kind(), io::ErrorKind::PermissionDenied),
            FailedStep::Write => assert_eq!(own.raw_os_error(), Some(9), "{step:?}"),
            _ => {}
        }
        let swept = store.sweep_temporaries(false).unwrap();
        assert!(swept[0].removed, "{step:?}: {swept:?}");
        assert!(store.temporaries().unwrap().is_empty(), "{step:?}");
    }
}

/// A sweep whose removal and then directory sync both fail reports both: the removal's error, with the sync's added
/// (ROADMAP 2.3.12). One stale file is removed first, so the directory needs its sync.
#[test]
fn a_sweep_whose_removal_and_sync_failed_reports_both() {
    let Some(dir) = local_tempdir() else { return };
    let root = dir.path().join("store");
    let fs = ForkedFs::default();
    let store = LocalStorage::open_in(fs.clone(), &root).unwrap();
    for n in 0..2 {
        std::fs::write(root.join(format!("@tmp.0123456789abcdef.{n}")), b"stale").unwrap();
    }
    fs.passing_removes.store(1, Ordering::Relaxed);
    fs.fail_removes.store(true, Ordering::Relaxed);
    fs.fail_syncs.store(true, Ordering::Relaxed);
    let err = store.sweep_temporaries(false).unwrap_err();
    let messages = chain(&err).join(" | ");
    assert!(messages.contains("injected: remove failed"), "{messages}");
    assert!(messages.contains("injected: sync failed"), "{messages}");
    assert!(
        std::error::Error::source(&err).is_some_and(|s| s.to_string().contains("remove failed")),
        "the removal's error is not the source: {messages}"
    );
}

/// The same for a container's sweep of its side files (ROADMAP 2.3.12).
#[test]
fn a_container_sweep_whose_removal_and_sync_failed_reports_both() {
    let Some(dir) = local_tempdir() else { return };
    let dir_path = std::fs::canonicalize(dir.path()).unwrap();
    let path = dir_path.join("app.persia");
    drop(Container::create(&path).unwrap());
    for n in 0..2 {
        std::fs::write(
            dir_path.join(format!("app.persia.tmp.0123456789abcdef.{n}")),
            b"x",
        )
        .unwrap();
    }
    let fs = ForkedFs::default();
    let c = Container::open_in(Arc::new(fs.clone()), &path, Mode::ReadWrite).unwrap();
    fs.passing_removes.store(1, Ordering::Relaxed);
    fs.fail_removes.store(true, Ordering::Relaxed);
    fs.fail_syncs.store(true, Ordering::Relaxed);
    let err = c.sweep_stale_files().unwrap_err();
    let messages = chain(&err).join(" | ");
    assert!(messages.contains("injected: remove failed"), "{messages}");
    assert!(messages.contains("injected: sync failed"), "{messages}");
    // `Error::Io` → the combined `io::Error` → the removal's own error.
    let removal = std::error::Error::source(&err)
        .and_then(std::error::Error::source)
        .map(ToString::to_string);
    assert_eq!(
        removal.as_deref(),
        Some("injected: remove failed"),
        "the removal's error is not the source: {messages}"
    );
}
