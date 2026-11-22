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
                .compact(&[], |_| Ok(b"c".to_vec()), Compression::None)
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

// --- The blob pack's side files (ROADMAP 9.2.10; SPEC §9.3) ---

const PACK_LOCK: &str = "/db/app.persia.blobs.lock";
const LEASES: &str = "/db/app.persia.blobs.leases";
const PACK_STALE: [&str; 2] = [
    "/db/app.persia.blobs.tmp.0123456789abcdef.0",
    "/db/app.persia.blobs.tmp.fedcba9876543210.17",
];

/// A container, two stale pack temporary files, names a pack never makes, the pack itself, and a lease directory with
/// a lease in it: all durable.
fn with_pack_side_files() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.sync_dir(Path::new("/")).unwrap();
    drop(Container::create_in(vfs(&fs), DB).unwrap());
    for name in PACK_STALE.into_iter().chain([
        "/db/app.persia.blobs",
        "/db/app.persia.blobs.tmp.0123456789ABCDEF.1",
        "/db/app.persia.blobs.tmp.0123456789abcdef",
        "/db/app.persia.blobs.tmp.0123456789abcdef.1x",
        "/db/app.persia.blobs.compact.0123456789abcdef.1",
        "/db/app.persia.blobs.lock.tmp.0123456789abcdef.1",
        "/db/other.persia.blobs.tmp.0123456789abcdef.1",
    ]) {
        let f = fs.create_new(Path::new(name)).unwrap();
        f.write_all_at(b"left by a crash", 0).unwrap();
        f.sync_data().unwrap();
    }
    // A directory with a temporary file's name is not one.
    fs.create_dir(Path::new("/db/app.persia.blobs.tmp.aaaaaaaaaaaaaaaa.2"))
        .unwrap();
    fs.create_dir(Path::new(LEASES)).unwrap();
    fs.create_new(Path::new(&format!(
        "{LEASES}/0123456789abcdef0123456789abcdef.lease"
    )))
    .unwrap()
    .sync_data()
    .unwrap();
    fs.sync_dir(Path::new(LEASES)).unwrap();
    fs.sync_dir(Path::new("/db")).unwrap();
    fs
}

fn listed(fs: &SimFs) -> Vec<(String, SideFileKind, InUse)> {
    Container::side_files_in(fs, DB)
        .unwrap()
        .into_iter()
        .map(|f| {
            assert!(!f.removed);
            (f.path.to_string_lossy().into_owned(), f.kind, f.in_use)
        })
        .collect()
}

/// `side_files` lists the pack's temporary files by the pack's own strict rule (regular files, lowercase hex, the
/// exact name), its lock file and its lease directory; the sweep removes exactly the stale temporary files, under the
/// pack's writer lock (whose file it creates), with one directory sync, and never the lock or the leases.
#[test]
fn the_packs_stale_temporary_files_are_listed_and_swept_and_its_lock_and_leases_kept() {
    let fs = with_pack_side_files();
    let before = names(&fs);
    let want = |lock: Option<InUse>| {
        let mut want = vec![
            (PACK_STALE[0].to_owned(), SideFileKind::Temporary, InUse::No),
            (PACK_STALE[1].to_owned(), SideFileKind::Temporary, InUse::No),
            (LEASES.to_owned(), SideFileKind::Leases, InUse::Unknown),
        ];
        want.extend(lock.map(|l| (PACK_LOCK.to_owned(), SideFileKind::Lock, l)));
        want.sort_by(|a, b| a.0.cmp(&b.0));
        want
    };
    // No lock file: no pack writer has run, so the temporary files are free.
    assert_eq!(listed(&fs), want(None));
    assert_eq!(
        names(&fs),
        before,
        "listing removes nothing and creates no lock"
    );

    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    let mut removed = c.sweep_stale_files().unwrap();
    removed.sort();
    assert_eq!(removed, PACK_STALE.map(PathBuf::from));
    // Durable: one directory sync covered both removals.
    let after = names(&fs.crash(CrashLoss::All));
    for name in &before {
        let stale = PACK_STALE.iter().any(|s| s.ends_with(name.as_str()));
        assert_eq!(after.contains(name), !stale, "{name}: {after:?}");
    }
    assert!(
        fs.read(Path::new(PACK_LOCK)).is_some(),
        "the sweep created the lock file"
    );
    assert!(
        fs.read(Path::new(&format!(
            "{LEASES}/0123456789abcdef0123456789abcdef.lease"
        )))
        .is_some(),
        "a lease is never touched"
    );
    // The sweep released the pack's lock.
    assert!(
        fs.open(Path::new(PACK_LOCK), true)
            .unwrap()
            .try_lock()
            .unwrap()
    );
    assert_eq!(
        listed(&fs),
        [
            (LEASES.to_owned(), SideFileKind::Leases, InUse::Unknown),
            (PACK_LOCK.to_owned(), SideFileKind::Lock, InUse::No),
        ]
    );
    assert_eq!(
        c.sweep_stale_files().unwrap(),
        Vec::<PathBuf>::new(),
        "idempotent"
    );
    assert!(
        fs.read(Path::new(PACK_LOCK)).is_some(),
        "the lock file is never removed"
    );
}

/// While a pack writer holds the pack's lock (a pack handle that outlived its database), the pack's temporary files
/// and its lock are in use: the listing says so and the sweep keeps them, removing only the container's own; the
/// lock stays the writer's.
#[test]
fn a_held_pack_lock_keeps_the_packs_temporary_files() {
    let fs = with_pack_side_files();
    let writer = fs.create_new(Path::new(PACK_LOCK)).unwrap();
    assert!(writer.try_lock().unwrap());
    let container_stale = "/db/app.persia.tmp.0123456789abcdef.4";
    fs.create_new(Path::new(container_stale)).unwrap();
    assert_eq!(
        listed(&fs),
        [
            (LEASES.to_owned(), SideFileKind::Leases, InUse::Unknown),
            (PACK_LOCK.to_owned(), SideFileKind::Lock, InUse::Yes),
            (
                PACK_STALE[0].to_owned(),
                SideFileKind::Temporary,
                InUse::Yes
            ),
            (
                PACK_STALE[1].to_owned(),
                SideFileKind::Temporary,
                InUse::Yes
            ),
            (
                container_stale.to_owned(),
                SideFileKind::Temporary,
                InUse::No
            ),
        ]
    );
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert_eq!(
        c.sweep_stale_files().unwrap(),
        [PathBuf::from(container_stale)]
    );
    for stale in PACK_STALE {
        assert!(fs.read(Path::new(stale)).is_some(), "{stale} kept");
    }
    assert!(
        !fs.open(Path::new(PACK_LOCK), true)
            .unwrap()
            .try_lock()
            .unwrap(),
        "still the writer's"
    );
    drop(writer);
    assert_eq!(c.sweep_stale_files().unwrap().len(), 2);
}

/// Where locks belong to the process (ADR-0019), a pack lock taken proves nothing: the pack's temporary files are
/// listed as unknown and kept.
#[test]
fn with_per_process_locks_no_pack_temporary_file_is_swept() {
    let fs = with_pack_side_files();
    fs.set_per_process_locks(true);
    let c = Container::open_in(vfs(&fs), DB, Mode::ReadWrite).unwrap();
    assert_eq!(c.sweep_stale_files().unwrap(), Vec::<PathBuf>::new());
    for stale in PACK_STALE {
        assert!(fs.read(Path::new(stale)).is_some(), "{stale} kept");
    }
    let files = Container::side_files_in(&fs, DB).unwrap();
    assert!(
        files
            .iter()
            .filter(|f| f.kind != SideFileKind::Leases)
            .all(|f| f.in_use == InUse::Unknown),
        "{files:?}"
    );
}

/// ROADMAP 9.2.10 on the operating system's file system: symbolic links named like the pack's temporary file, lock
/// file and lease directory, pointing outside the directory, are none of the pack's side files. The listing leaves them
/// out, the sweep removes only the real temporary file beside them, and neither a link nor what it points to is
/// touched.
#[cfg(unix)]
#[test]
fn symlinks_named_like_the_packs_side_files_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.persia");
    drop(Container::create(&path).unwrap());
    let target_file = outside.path().join("precious");
    std::fs::write(&target_file, b"not the pack's").unwrap();
    let target_dir = outside.path().join("dir");
    std::fs::create_dir(&target_dir).unwrap();
    std::fs::write(target_dir.join("kept"), b"kept").unwrap();
    let links = [
        ("app.persia.blobs.tmp.0123456789abcdef.1", &target_file),
        ("app.persia.blobs.tmp.0123456789abcdef.2", &target_dir),
        ("app.persia.blobs.lock", &target_file),
        ("app.persia.blobs.leases", &target_dir),
    ];
    for (name, target) in links {
        std::os::unix::fs::symlink(target, dir.path().join(name)).unwrap();
    }
    let stale = dir.path().join("app.persia.blobs.tmp.fedcba9876543210.3");
    std::fs::write(&stale, b"left by a crash").unwrap();

    let listed: Vec<PathBuf> = Container::side_files(&path)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    // The listing canonicalizes the container's path (a temporary directory may sit behind a link).
    let real_dir = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(listed, [real_dir.join(stale.file_name().unwrap())]);
    let c = Container::open(&path, Mode::ReadWrite).unwrap();
    assert_eq!(
        c.sweep_stale_files().unwrap(),
        [real_dir.join(stale.file_name().unwrap())]
    );
    assert!(!stale.exists());
    for (name, _) in links {
        let meta = std::fs::symlink_metadata(dir.path().join(name)).unwrap();
        assert!(meta.file_type().is_symlink(), "{name}");
    }
    assert_eq!(std::fs::read(&target_file).unwrap(), b"not the pack's");
    assert_eq!(std::fs::read(target_dir.join("kept")).unwrap(), b"kept");
}
