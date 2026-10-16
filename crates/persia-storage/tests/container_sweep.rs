//! Sweeping the temporary files of crashed creates and compactions (ROADMAP 2.4.15; SPEC §4.5.1, §4.6).
//!
//! Invariants: `sweep_stale_files` removes exactly this container's `<name>.tmp.*` and `<name>.compact.*` files whose
//! writer lock is free, durably; it keeps everything else (the container, other containers' files, quarantine files,
//! a temporary file a live create still locks), and needs a read-write handle opened by path.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Commit, Container, ContainerFile, Error, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};

const DB: &str = "/db/app.persia";

fn vfs(fs: &SimFs) -> Arc<dyn Vfs<File = SimFsFile>> {
    Arc::new(fs.clone())
}

fn names(fs: &SimFs) -> Vec<String> {
    fs.files()
        .into_iter()
        .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// A file system where a create of `DB` crashed after writing its temporary file, then a create succeeded, then a
/// compaction crashed before its rename: one stale file of each kind, durable.
fn with_stale_files() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    // Fails at the link (create_new, header write, sync, link): the temporary file is complete and stays.
    let start = fs.mutations();
    fs.set_fault(Some(SimFault::FailAt(start + 3)));
    assert!(Container::create_in(vfs(&fs), DB).is_err());
    let fs = fs.crash(CrashLoss::Nothing);
    assert_eq!(names(&fs).len(), 1, "{:?}", names(&fs));
    let mut c = Container::create_in(vfs(&fs), DB).unwrap();
    let m = c
        .append(FrameKind::Manifest, b"m", Compression::None)
        .unwrap();
    c.commit(Commit {
        manifest: m,
        wal_start: m.end(),
        fence_token: 0,
    })
    .unwrap();
    drop(c);
    // A compaction that dies before its rename: the first failing call that leaves a `.compact` file behind.
    let fs = (0..100)
        .find_map(|k| {
            let copy = fs.crash(CrashLoss::Nothing);
            let mut c = Container::open_in(vfs(&copy), DB, Mode::ReadWrite).unwrap();
            copy.set_fault(Some(SimFault::FailAt(copy.mutations() + k)));
            let failed = c
                .compact(&[], |_| b"c".to_vec(), Compression::None)
                .is_err();
            drop(c);
            let after = copy.crash(CrashLoss::Nothing);
            (failed && names(&after).iter().any(|n| n.contains(".compact."))).then_some(after)
        })
        .unwrap();
    // Files that are not this container's temporary files.
    for other in [
        "/db/app.persia.quarantine.0000000000003028.0123456789abcdef.1",
        "/db/other.persia.tmp.0123456789abcdef.1",
    ] {
        fs.create_new(Path::new(other)).unwrap();
    }
    fs.sync_dir(Path::new("/db")).unwrap();
    fs
}

#[test]
fn stale_temporary_files_are_removed_durably_and_nothing_else() {
    let fs = with_stale_files();
    let before = names(&fs);
    let stale: Vec<&String> = before
        .iter()
        .filter(|n| n.starts_with("app.persia.tmp.") || n.starts_with("app.persia.compact."))
        .collect();
    assert_eq!(stale.len(), 2, "{before:?}");
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    let mut removed: Vec<PathBuf> = c.sweep_stale_files().unwrap();
    removed.sort();
    let mut want: Vec<PathBuf> = stale.iter().map(|n| Path::new("/db").join(n)).collect();
    want.sort();
    assert_eq!(removed, want);
    let kept = names(&fs.crash(CrashLoss::All));
    assert_eq!(
        kept,
        [
            "app.persia",
            "app.persia.quarantine.0000000000003028.0123456789abcdef.1",
            "other.persia.tmp.0123456789abcdef.1"
        ]
    );
    assert_eq!(
        c.sweep_stale_files().unwrap(),
        Vec::<PathBuf>::new(),
        "idempotent"
    );
}

#[test]
fn a_temporary_file_locked_by_a_live_create_is_kept() {
    let fs = with_stale_files();
    let live = Path::new("/db/app.persia.tmp.00000000000000ff.7");
    let held = fs.create_new(live).unwrap();
    assert!(held.try_lock().unwrap());
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    let removed = c.sweep_stale_files().unwrap();
    assert_eq!(removed.len(), 2);
    assert!(fs.read(live).is_some());
}

#[test]
fn sweeping_needs_a_writer_opened_by_path() {
    let fs = with_stale_files();
    let ro = Container::open_in(vfs(&fs), DB, Mode::ReadOnly).unwrap();
    assert!(matches!(
        ro.sweep_stale_files(),
        Err(Error::ReadOnly { .. })
    ));
    let bare = Container::create_on(SimFile::new(), [1; 16]).unwrap();
    assert!(matches!(
        bare.sweep_stale_files(),
        Err(Error::InvalidInput(_))
    ));
}

#[test]
fn sweeping_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(Container::create(&path).unwrap());
    let stale = dir.path().join("app.persia.compact.0123456789abcdef.3");
    std::fs::write(&stale, b"half a container").unwrap();
    let c = Container::open(&path, Mode::ReadWrite).unwrap();
    assert_eq!(c.sweep_stale_files().unwrap(), std::slice::from_ref(&stale));
    assert!(!stale.exists());
}
