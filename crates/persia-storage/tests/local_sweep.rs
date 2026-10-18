//! Sweeping the temporary files of crashed `put`s from a local store (ROADMAP 2.3.8; SPEC §10.1.1, ADR-0012).
//!
//! Invariants:
//! 1. `sweep_temporaries` removes exactly the `@tmp.<16 hex>.<n>` files, at any depth, that no `put` holds; objects,
//!    foreign files, look-alike names and anything behind a symbolic link are untouched, and `list` is unchanged.
//! 2. A temporary file whose lock a live writer holds is reported in use and kept; `temporaries` removes nothing.
//! 3. A sweep racing with `put`s and `put_if_absent`s never makes one fail, never loses an acknowledged object, and
//!    leaves no temporary file behind once the writers are done (the lock-then-check handshake of `write_temp`).
//! 4. Removals are durable: the directory is synced (checked on `SimFs` by crashing after the sweep).

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
// Most cases need file locks, which Windows does not take (ADR-0018): their helpers are unused there.
#![cfg_attr(windows, allow(dead_code, unused_imports))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use persia_storage::container::ContainerFile;
use persia_storage::container::sim::CrashLoss;
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::SimFs;
use persia_storage::{InUse, Key, KeyPrefix, LocalStorage, PutOutcome, SideFileKind, Storage};

fn key(s: &str) -> Key {
    Key::new(s).unwrap()
}

fn keys<S: Storage>(s: &S) -> Vec<String> {
    s.list(&KeyPrefix::new("").unwrap())
        .unwrap()
        .into_iter()
        .map(|m| m.key.as_str().to_owned())
        .collect()
}

/// Every file under `dir` (relative paths), symbolic links not followed.
fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let e = e.unwrap();
            let t = e.file_type().unwrap();
            if t.is_dir() {
                dirs.push(e.path());
            }
            out.push(e.path().strip_prefix(dir).unwrap().display().to_string());
        }
    }
    out.sort();
    out
}

const STALE: [&str; 3] = [
    "@tmp.0123456789abcdef.0",
    "segments@/@tmp.fedcba9876543210.7",
    "segments@/c1@/@tmp.00000000000000aa.12",
];

/// A store with objects at two depths, a stale temporary file at three depths, and look-alikes to keep.
fn littered() -> (tempfile::TempDir, LocalStorage) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStorage::open(dir.path()).unwrap();
    store
        .put(&key("m"), Bytes::from_static(b"manifest"))
        .unwrap();
    store
        .put(&key("segments/c1/1.seg"), Bytes::from_static(b"seg"))
        .unwrap();
    for stale in STALE {
        std::fs::write(dir.path().join(stale), b"crashed put").unwrap();
    }
    for keep in [
        "@tmp.0123456789abcdef",
        "@tmp.0123456789abcdeg.1",
        "@tmp.0123456789abcdef.1x",
        "segments@/notes.txt",
        "segments@/tmp.0123456789abcdef.1",
    ] {
        std::fs::write(dir.path().join(keep), b"keep").unwrap();
    }
    (dir, store)
}

#[cfg(unix)]
#[test]
fn a_sweep_removes_stale_temporaries_at_every_depth_and_nothing_else() {
    let (dir, store) = littered();
    // A temporary-looking file behind a symbolic link to a directory outside the store is never reached.
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("@tmp.0123456789abcdef.3"), b"not ours").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked@")).unwrap();
    let before = tree(dir.path());
    let listed = keys(&store);

    let swept = store.sweep_temporaries(false).unwrap();
    let want: Vec<PathBuf> = {
        let mut v: Vec<_> = STALE.iter().map(|s| dir.path().join(s)).collect();
        v.sort();
        v
    };
    assert_eq!(
        swept.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        want
    );
    for f in &swept {
        assert_eq!(
            (f.kind, f.in_use, f.removed),
            (SideFileKind::Temporary, InUse::No, true),
            "{f:?}"
        );
    }
    let after = tree(dir.path());
    let gone: Vec<_> = before
        .iter()
        .filter(|p| !after.contains(p))
        .cloned()
        .collect();
    assert_eq!(
        gone,
        STALE.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
    );
    assert_eq!(keys(&store), listed);
    assert!(outside.path().join("@tmp.0123456789abcdef.3").exists());
    assert_eq!(
        store.sweep_temporaries(false).unwrap(),
        [],
        "a second sweep finds nothing"
    );
}

#[cfg(unix)]
#[test]
fn a_temporary_a_writer_holds_is_kept_and_listing_removes_nothing() {
    let (dir, store) = littered();
    let held = std::fs::File::open(dir.path().join(STALE[1])).unwrap();
    assert!(ContainerFile::try_lock(&held).unwrap());

    let listed = store.temporaries().unwrap();
    assert_eq!(listed.len(), STALE.len());
    assert!(listed.iter().all(|f| !f.removed));
    let in_use: Vec<_> = listed.iter().filter(|f| f.in_use == InUse::Yes).collect();
    assert_eq!(in_use.len(), 1);
    assert_eq!(in_use[0].path, dir.path().join(STALE[1]));
    for s in STALE {
        assert!(dir.path().join(s).exists(), "{s}");
    }

    let swept = store.sweep_temporaries(true).unwrap();
    assert_eq!(swept.iter().filter(|f| f.removed).count(), STALE.len() - 1);
    assert!(
        dir.path().join(STALE[1]).exists(),
        "held by a writer, even with force"
    );
    drop(held);
    assert_eq!(store.sweep_temporaries(false).unwrap().len(), 1);
    assert!(!dir.path().join(STALE[1]).exists());
}

/// Writers `put` and `put_if_absent` while a sweeper sweeps in a loop: no write fails, every object reads back, and
/// no temporary file is left. Without the lock, or without the check that the name is still the writer's after
/// taking it, a sweep removes a live temporary file and its write fails.
#[cfg(unix)]
#[test]
fn sweeps_racing_with_puts_never_fail_or_lose_a_write() {
    const WRITERS: usize = 4;
    const PUTS: usize = 300;
    let dir = tempfile::tempdir().unwrap();
    let store = LocalStorage::open(dir.path()).unwrap();
    let done = AtomicBool::new(false);
    let swept = std::thread::scope(|s| {
        let sweeper = s.spawn(|| {
            let mut removed = 0;
            while !done.load(Ordering::Relaxed) {
                removed += store
                    .sweep_temporaries(false)
                    .unwrap()
                    .iter()
                    .filter(|f| f.removed)
                    .count();
            }
            removed
        });
        let writers: Vec<_> = (0..WRITERS)
            .map(|w| {
                let store = &store;
                s.spawn(move || {
                    for i in 0..PUTS {
                        let k = key(&format!("w{w}/{}/{i}", i % 3));
                        let data = Bytes::from(format!("{w}:{i}").repeat(1 + i % 50));
                        if i % 2 == 0 {
                            store.put(&k, data).unwrap();
                        } else {
                            assert_eq!(store.put_if_absent(&k, data).unwrap(), PutOutcome::Created);
                        }
                    }
                })
            })
            .collect();
        for w in writers {
            w.join().unwrap();
        }
        done.store(true, Ordering::Relaxed);
        sweeper.join().unwrap()
    });
    // A sweep may take a file in the instant between its creation and its writer's lock: that writer then takes
    // another name, so the removal costs nothing.
    eprintln!("{swept} temporary files taken before their writer locked them");
    for w in 0..WRITERS {
        for i in 0..PUTS {
            let k = key(&format!("w{w}/{}/{i}", i % 3));
            assert_eq!(
                store.get(&k).unwrap(),
                format!("{w}:{i}").repeat(1 + i % 50).as_bytes()
            );
        }
    }
    assert_eq!(store.temporaries().unwrap(), []);
}

/// On `SimFs`: a stale temporary file is removed durably (the directory is synced), and one a writer holds stays.
#[test]
fn a_sweep_is_durable_on_a_simulated_file_system() {
    let fs = SimFs::new();
    let store = LocalStorage::open_in(fs.clone(), Path::new("/store")).unwrap();
    // As the store made it absolute (`D:\store` on Windows), so the paths it reports compare equal.
    let root = store.root().to_owned();
    store.put(&key("a/b"), Bytes::from_static(b"x")).unwrap();
    let stale = root.join("a@").join("@tmp.0123456789abcdef.0");
    let held = root.join("a@").join("@tmp.0123456789abcdef.1");
    for p in [&stale, &held] {
        fs.create_new(p).unwrap().sync_all().unwrap();
    }
    fs.sync_dir(&root.join("a@")).unwrap();
    let lock = fs.open(&held, false).unwrap();
    assert!(lock.try_lock().unwrap());

    let swept = store.sweep_temporaries(false).unwrap();
    assert_eq!(
        swept
            .iter()
            .map(|f| (f.path.clone(), f.in_use, f.removed))
            .collect::<Vec<_>>(),
        vec![
            (stale.clone(), InUse::No, true),
            (held.clone(), InUse::Yes, false)
        ]
    );
    let after = fs.crash(CrashLoss::All);
    let names: Vec<PathBuf> = after.files().into_iter().map(|(p, _)| p).collect();
    assert!(
        !names.contains(&stale),
        "the removal survived the crash: {names:?}"
    );
    assert!(names.contains(&held));
    let reopened = LocalStorage::open_in(after, &root).unwrap();
    assert_eq!(&*reopened.get(&key("a/b")).unwrap(), b"x");
}

/// What a sweep racing with `write_temp` does right after the temporary file is created, before its writer locks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Race {
    /// It locked the file and removed it: the writer's lock succeeds on a file that no longer has the name.
    Removed,
    /// It holds the lock and removed the file, and has not released the lock yet: the writer's lock fails.
    Holding,
    /// A listing holds the lock for an instant and removes nothing: the writer's lock fails, and the file would stay
    /// behind unless the writer removes it.
    Listed,
}

/// A `SimFs` whose next `create_new` of a temporary file is interfered with as in [`Race`].
#[derive(Debug, Clone)]
struct Racing {
    fs: SimFs,
    race: Arc<Mutex<Option<Race>>>,
    /// The handle of a sweep that holds a lock ([`Race::Holding`]).
    held: Arc<Mutex<Vec<persia_storage::vfs::sim::SimFsFile>>>,
}

impl Vfs for Racing {
    type File = persia_storage::vfs::sim::SimFsFile;

    fn create_new(&self, path: &Path) -> std::io::Result<Self::File> {
        let file = self.fs.create_new(path)?;
        let temp = path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("@tmp.");
        match self.race.lock().unwrap().take_if(|_| temp) {
            Some(Race::Removed) => self.fs.remove_file(path)?,
            Some(Race::Listed) => {
                let listing = self.fs.open(path, false)?;
                assert!(listing.try_lock()?);
                self.held.lock().unwrap().push(listing);
            }
            Some(Race::Holding) => {
                let sweep = self.fs.open(path, false)?;
                assert!(sweep.try_lock()?);
                self.fs.remove_file(path)?;
                self.held.lock().unwrap().push(sweep);
            }
            None => {}
        }
        Ok(file)
    }

    fn open(&self, path: &Path, write: bool) -> std::io::Result<Self::File> {
        self.fs.open(path, write)
    }
    fn create_dir(&self, path: &Path) -> std::io::Result<()> {
        self.fs.create_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.fs.rename(from, to)
    }
    fn hard_link(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.fs.hard_link(from, to)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.fs.remove_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        self.fs.sync_dir(dir)
    }
    fn entry_kind(&self, path: &Path) -> std::io::Result<persia_storage::vfs::EntryKind> {
        self.fs.entry_kind(path)
    }
    fn is_dir(&self, path: &Path) -> std::io::Result<bool> {
        self.fs.is_dir(path)
    }
    fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<persia_storage::vfs::DirEntry>> {
        self.fs.read_dir(dir)
    }
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.fs.canonicalize(path)
    }
    fn same_file(&self, file: &Self::File, path: &Path) -> std::io::Result<bool> {
        self.fs.same_file(file, path)
    }
    fn lock_beside(&self, path: &Path) -> std::io::Result<Option<Self::File>> {
        self.fs.lock_beside(path)
    }
}

/// The ways a sweep or a listing can take a temporary file before its writer locks it: each time the writer notices, takes
/// another name, and its `put` or `put_if_absent` succeeds with nothing left behind.
#[test]
fn a_put_whose_temporary_a_sweep_took_retries_under_another_name() {
    for race in [Race::Removed, Race::Holding, Race::Listed] {
        for if_absent in [false, true] {
            let ctx = format!("{race:?}, put_if_absent: {if_absent}");
            let fs = SimFs::new();
            let vfs = Racing {
                fs,
                race: Arc::default(),
                held: Arc::default(),
            };
            let store = LocalStorage::open_in(vfs.clone(), Path::new("/store")).unwrap();
            let k = key("a/b");
            // Create the directory first, so the race hits the object's temporary file.
            store
                .put(&key("a/first"), Bytes::from_static(b"0"))
                .unwrap();
            *vfs.race.lock().unwrap() = Some(race);
            if if_absent {
                assert_eq!(
                    store.put_if_absent(&k, Bytes::from_static(b"x")).unwrap(),
                    PutOutcome::Created,
                    "{ctx}"
                );
            } else {
                store.put(&k, Bytes::from_static(b"x")).unwrap();
            }
            assert_eq!(&*store.get(&k).unwrap(), b"x", "{ctx}");
            assert_eq!(*vfs.race.lock().unwrap(), None, "{ctx}: the race happened");
            // The sweep that held a lock releases it; nothing is left.
            drop(std::mem::take(&mut *vfs.held.lock().unwrap()));
            assert_eq!(
                store.temporaries().unwrap(),
                [],
                "{ctx}: nothing left behind"
            );
            let left: Vec<_> = store.sweep_temporaries(false).unwrap();
            assert!(left.iter().all(|f| f.removed), "{ctx}: {left:?}");
            assert_eq!(store.temporaries().unwrap(), [], "{ctx}");
        }
    }
}

/// Windows locks no files, so whether a temporary file is in use is unknown: a sweep keeps every one unless forced.
#[cfg(windows)]
#[test]
fn on_windows_only_a_forced_sweep_removes_temporaries() {
    let (dir, store) = littered();
    let kept = store.sweep_temporaries(false).unwrap();
    assert_eq!(kept.len(), STALE.len());
    assert!(
        kept.iter()
            .all(|f| f.in_use == InUse::Unknown && !f.removed),
        "{kept:?}"
    );
    for s in STALE {
        assert!(dir.path().join(s).exists(), "{s}");
    }
    let forced = store.sweep_temporaries(true).unwrap();
    assert!(forced.iter().all(|f| f.removed), "{forced:?}");
    assert_eq!(store.temporaries().unwrap(), []);
}
