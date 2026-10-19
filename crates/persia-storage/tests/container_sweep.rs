//! Sweeping the temporary files of crashed creates and compactions (ROADMAP 2.4.15; SPEC §4.5.1, §4.6).
//!
//! Invariants: `sweep_stale_files` removes exactly this container's `<name>.tmp.*` and `<name>.compact.*` files whose
//! writer lock is free, durably; it keeps everything else (the container, other containers' files, quarantine files,
//! a temporary file a live create still locks), and needs a read-write handle opened by path. `side_files_in` lists
//! this container's temporary and quarantine files without removing any, with whether a writer holds each, and
//! `remove_quarantine_files` removes the quarantine copies durably (ROADMAP 2.4.19).

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use persia_format::block::Compression;
use persia_format::frame::FrameKind;
use persia_storage::container::sim::{CrashLoss, SimFault, SimFile};
use persia_storage::container::{Commit, Container, ContainerFile, Error, Mode};
use persia_storage::vfs::Vfs;
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use persia_storage::{InUse, SideFileKind};

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
    // Canonical, as the container reports it (macOS `/var` is a symbolic link to `/private/var`).
    let dir_path = std::fs::canonicalize(dir.path()).unwrap();
    let path = dir_path.join("app.persia");
    drop(Container::create(&path).unwrap());
    let stale = dir_path.join("app.persia.compact.0123456789abcdef.3");
    std::fs::write(&stale, b"half a container").unwrap();
    let c = Container::open(&path, Mode::ReadWrite).unwrap();
    assert_eq!(c.sweep_stale_files().unwrap(), std::slice::from_ref(&stale));
    assert!(!stale.exists());
}

const QUARANTINE: &str = "/db/app.persia.quarantine.0000000000003028.0123456789abcdef.1";

#[test]
fn side_files_lists_temporaries_and_quarantines_and_removes_nothing() {
    let fs = with_stale_files();
    let live = Path::new("/db/app.persia.tmp.00000000000000ff.7");
    let held = fs.create_new(live).unwrap();
    assert!(held.try_lock().unwrap());
    let before = names(&fs);

    let files = Container::side_files_in(&fs, DB).unwrap();
    let got: Vec<(String, SideFileKind, InUse, bool)> = files
        .iter()
        .map(|f| {
            let name = f.path.file_name().unwrap().to_string_lossy().into_owned();
            (name, f.kind, f.in_use, f.removed)
        })
        .collect();
    let mut want: Vec<(String, SideFileKind, InUse, bool)> = before
        .iter()
        .filter(|n| n.starts_with("app.persia.") && *n != "app.persia")
        .map(|n| {
            let kind = if n.contains(".quarantine.") {
                SideFileKind::Quarantine
            } else {
                SideFileKind::Temporary
            };
            // By name, not by the `.7` suffix: the counter is per process, so a stale file can end in `.7` too.
            let in_use = if live.file_name().is_some_and(|l| l == n.as_str()) {
                InUse::Yes
            } else {
                InUse::No
            };
            (n.clone(), kind, in_use, false)
        })
        .collect();
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(got, want);
    assert_eq!(got.len(), 4, "two stale, one live, one quarantine: {got:?}");
    assert_eq!(names(&fs), before, "listing removes nothing");
    // A missing container has no side files, and its directory still lists.
    assert_eq!(
        Container::side_files_in(&fs, "/db/missing.persia").unwrap(),
        []
    );
}

#[test]
fn quarantine_files_are_removed_durably_by_a_writer_only() {
    let fs = with_stale_files();
    let q = [PathBuf::from(QUARANTINE)];
    let ro = Container::open_in(vfs(&fs), DB, Mode::ReadOnly).unwrap();
    assert!(matches!(
        ro.remove_quarantine_files(&q),
        Err(Error::ReadOnly { .. })
    ));
    drop(ro);
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    // Only this container's quarantine files: a temporary file, another container's file or another directory's
    // are refused, and nothing is removed.
    let before = names(&fs);
    for bad in [
        "/db/app.persia.tmp.00000000000000ff.7",
        "/db/other.persia.quarantine.0000000000003028.0123456789abcdef.1",
        "/elsewhere/app.persia.quarantine.0000000000003028.0123456789abcdef.1",
        "/db/app.persia",
    ] {
        let paths = [q[0].clone(), PathBuf::from(bad)];
        assert!(
            matches!(
                c.remove_quarantine_files(&paths),
                Err(Error::InvalidInput(_))
            ),
            "{bad}"
        );
        assert_eq!(names(&fs), before, "{bad}");
    }
    assert_eq!(
        c.remove_quarantine_files(&[]).unwrap(),
        Vec::<PathBuf>::new()
    );
    assert_eq!(names(&fs), before, "an empty list removes nothing");
    assert_eq!(c.remove_quarantine_files(&q).unwrap(), q);
    let kept = names(&fs.crash(CrashLoss::All));
    assert!(!kept.iter().any(|n| n.contains(".quarantine.")), "{kept:?}");
    assert_eq!(
        kept.iter().filter(|n| n.starts_with("app.persia.")).count(),
        2,
        "the stale temporaries are a separate sweep: {kept:?}"
    );
    assert_eq!(
        c.remove_quarantine_files(&q).unwrap(),
        Vec::<PathBuf>::new(),
        "idempotent"
    );
}

/// Where locks belong to the process (`flock` emulated on NFS, ADR-0019), the lock a sweep takes may be a live create's
/// of this same process: no temporary file is removed, and the listing says their use is unknown.
#[test]
fn with_per_process_locks_no_temporary_file_is_swept() {
    let fs = with_stale_files();
    fs.set_per_process_locks(true);
    let before = names(&fs);
    let files = Container::side_files_in(&fs, DB).unwrap();
    let temporaries: Vec<_> = files
        .iter()
        .filter(|f| f.kind == SideFileKind::Temporary)
        .collect();
    assert_eq!(temporaries.len(), 2, "{files:?}");
    assert!(
        temporaries.iter().all(|f| f.in_use == InUse::Unknown),
        "{files:?}"
    );
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert_eq!(c.sweep_stale_files().unwrap(), Vec::<PathBuf>::new());
    assert_eq!(names(&fs), before);
}
