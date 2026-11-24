//! `persia_storage::vfs::create_dir_all_synced` (SPEC §4.5.2, §10.1.1): the walk that creates a directory and its
//! missing ancestors durably.
//!
//! Invariants:
//! 1. Every directory it creates survives a crash that drops unsynced names: each one's parent is synced.
//! 2. A directory another process creates between the walk's check and its `create_dir` (`AlreadyExists`) is taken
//!    as created, and its parent is still synced.
//! 3. Any other failure of `create_dir` is returned as it is, not masked by a later step.

// Tests: inputs are fixed.
#![allow(clippy::unwrap_used)]
// The walk makes its path absolute with `std::path::absolute`, which gives `/data` a drive prefix on Windows that the
// simulated file system's paths do not have.
#![cfg(unix)]

use std::io;
use std::path::{Path, PathBuf};

use persia_storage::container::sim::{CrashLoss, SimFault};
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use persia_storage::vfs::{DirEntry, EntryKind, Vfs, create_dir_all_synced};

/// A `SimFs` whose `create_dir` is scripted: `LosesRaces` creates the directory, then reports `AlreadyExists`, as the
/// loser of a race with another process sees it; `Refuses` creates nothing and reports `PermissionDenied`.
#[derive(Debug)]
struct Scripted(SimFs, Script);

#[derive(Debug, Clone, Copy)]
enum Script {
    LosesRaces,
    Refuses,
}

impl Vfs for Scripted {
    type File = SimFsFile;

    fn open(&self, path: &Path, write: bool) -> io::Result<SimFsFile> {
        self.0.open(path, write)
    }
    fn create_new(&self, path: &Path) -> io::Result<SimFsFile> {
        self.0.create_new(path)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        match self.1 {
            Script::LosesRaces => {
                self.0.create_dir(path)?;
                Err(io::ErrorKind::AlreadyExists.into())
            }
            Script::Refuses => Err(io::ErrorKind::PermissionDenied.into()),
        }
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
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.0.sync_dir(dir)
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
    fn same_file(&self, file: &SimFsFile, path: &Path) -> io::Result<bool> {
        self.0.same_file(file, path)
    }
    fn lock_beside(&self, path: &Path) -> io::Result<Option<SimFsFile>> {
        self.0.lock_beside(path)
    }
}

fn base() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/data")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    fs
}

const NEW: [&str; 3] = ["/data/a", "/data/a/b", "/data/a/b/c"];

#[test]
fn created_directories_survive_a_crash() {
    let fs = base();
    let made = create_dir_all_synced(&fs, "/data/a/b/c").unwrap();
    assert_eq!(made, Path::new("/data/a/b/c"));
    let crashed = fs.crash(CrashLoss::All);
    for dir in NEW {
        assert!(crashed.is_dir(Path::new(dir)).unwrap(), "{dir}");
    }
}

#[test]
fn a_directory_another_process_created_is_taken_and_synced() {
    let racing = Scripted(base(), Script::LosesRaces);
    create_dir_all_synced(&racing, "/data/a/b/c").unwrap();
    let crashed = racing.0.crash(CrashLoss::All);
    for dir in NEW {
        assert!(crashed.is_dir(Path::new(dir)).unwrap(), "{dir}");
    }
}

#[test]
fn another_failure_of_create_dir_is_returned_as_it_is() {
    // Refused: its own kind, not the `NotFound` the final check would meet had the walk gone on.
    let refusing = Scripted(base(), Script::Refuses);
    let err = create_dir_all_synced(&refusing, "/data/a/b/c").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert!(refusing.0.is_dir(Path::new("/data/a")).is_err());
    // An injected failure of the first `create_dir`, the next mutating call, is returned too, and nothing is made.
    let fs = base();
    fs.set_fault(Some(SimFault::FailAt(fs.mutations())));
    assert!(create_dir_all_synced(&fs, "/data/a/b/c").is_err());
    fs.set_fault(None);
    assert!(fs.is_dir(Path::new("/data/a")).is_err());
}
