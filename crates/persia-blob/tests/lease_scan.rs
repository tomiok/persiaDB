//! Reader leases on a simulated file system (SPEC §9.3, §9.5; ROADMAP 9.2.16; ADR-0054 items 4, 8, 9, 11, 12).
//!
//! Invariants:
//! 1. A take writes the opening record, then locks it: another handle is refused the lock. `set_open` rewrites the
//!    record in place; a drop removes the name and releases the lock.
//! 2. The writer's scan judges each reader lease by its lock: one it is granted is stale and removed, whatever its
//!    record; one held elsewhere is live and read. A live record that does not decode, names another lease id or
//!    another database is live and unreadable.
//! 3. Per entry: a name removed since the listing is skipped; a lock error is live and unreadable, never a failure;
//!    an entry that is not a regular file is never opened or removed.
//! 4. Where locks cannot be judged, nothing is opened or removed and every reader lease counts live; a pack whose file
//!    says so makes `PackStore::reader_leases` scan that way.
//! 5. The judgement: only a live reader lease open at the committed seq lets the sweep run; opening (0 included),
//!    behind, ahead and unreadable hold it back.
//! 6. A take starts again under a new id when its lock is refused, its sidecar is delete pending (Windows), or its
//!    name was removed before the lock, and fails with `LeaseContended` after 3 attempts, leaving nothing.
//! 7. With a lock sidecar (Windows' `lock_beside`, simulated), the record stays readable, a stale lease's sidecar is
//!    removed with it, and a sidecar whose lease is gone is removed.
//! 8. A directory that refuses writes gives a lease that holds nothing.
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    clippy::assert_is_empty,
    missing_docs
)] // Tests: inputs are fixed.

mod gc_support;
mod pack_support;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use persia_blob::pack::lease::{
    LEASE_LEN, LeaseRecord, LeaseScan, LiveLease, ReaderLease, ScannedLease, lease_file_name,
    leases_dir, parse_lease_name, scan_leases,
};
use persia_blob::{Error, PackMode};
use persia_storage::container::{Container, ContainerFile};
use persia_storage::vfs::sim::{SimFs, SimFsFile};
use persia_storage::vfs::{DirEntry, EntryKind, Vfs};

const DB: &str = "/db/app.persia";
const PACK: &str = "/db/app.persia.blobs";
const UUID: [u8; 16] = [7; 16];

/// A simulated file system with a container at [`DB`]: a take reopens it to read its committed seq.
fn sim() -> SimFs {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    drop(Container::create_in(Arc::new(fs.clone()), DB).unwrap());
    fs
}

fn take(vfs: &Arc<dyn Vfs<File = SimFsFile>>) -> ReaderLease<SimFsFile> {
    ReaderLease::take_in(vfs, Path::new(PACK), Path::new(DB)).unwrap()
}

fn record_at(fs: &SimFs, path: &Path) -> LeaseRecord {
    LeaseRecord::decode(&fs.read(path).unwrap()).unwrap()
}

fn lease_files(fs: &SimFs) -> Vec<String> {
    let mut names: Vec<String> = fs
        .read_dir(&leases_dir(Path::new(PACK)))
        .unwrap()
        .into_iter()
        .map(|e| e.name.into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// A reader lease file nobody holds, as a reader killed while holding it leaves: a valid open record, unlocked.
fn stale(fs: &SimFs, id: u128) -> PathBuf {
    let dir = leases_dir(Path::new(PACK));
    match fs.create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => panic!("{e}"),
    }
    let path = dir.join(lease_file_name(id));
    let file = fs.create_new(&path).unwrap();
    file.write_all_at(&LeaseRecord::open(id, 1, UUID, 3).encode(), 0)
        .unwrap();
    path
}

#[test]
fn a_take_writes_the_opening_record_locked_and_a_drop_removes_it() {
    let fs = sim();
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let mut lease = take(&vfs);
    assert!(lease.is_held());
    let (committed, _) =
        persia_blob::pack::lease::committed_seq_in(Arc::clone(&vfs), Path::new(DB)).unwrap();
    assert_eq!(
        lease.taken_seq(),
        committed,
        "the container's committed seq, read after the lock"
    );
    let path = lease.path().unwrap().to_owned();
    assert_eq!(path.parent().unwrap(), leases_dir(Path::new(PACK)));
    let id = parse_lease_name(path.file_name().unwrap()).unwrap();
    let record = record_at(&fs, &path);
    assert!(record.is_opening());
    assert_eq!(record.lease_id(), id);
    assert_eq!(record.pid(), std::process::id());
    assert!(
        !fs.open(&path, false).unwrap().try_lock().unwrap(),
        "another handle is refused the lock"
    );

    lease.set_open(UUID, 5);
    assert_eq!(
        record_at(&fs, &path),
        LeaseRecord::open(id, record.pid(), UUID, 5)
    );
    assert_eq!(lease.record(), Some(record_at(&fs, &path)));
    assert_eq!(fs.read(&path).unwrap().len(), LEASE_LEN);
    drop(lease);
    assert!(lease_files(&fs).is_empty(), "the name is removed");
    // The lock went with it: a file created at that name again is lockable.
    let again = fs.create_new(&path).unwrap();
    assert!(again.try_lock().unwrap());
}

#[test]
fn the_scan_judges_each_lease_by_its_lock() {
    let fs = sim();
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let mut live = take(&vfs);
    live.set_open(UUID, 5);
    let mut torn = take(&vfs);
    torn.set_open(UUID, 5);
    // A torn rewrite, as a read concurrent with the holder's write may see.
    fs.open(torn.path().unwrap(), true)
        .unwrap()
        .write_all_at(&[0xAB; 8], 20)
        .unwrap();
    let gone = stale(&fs, 0xdead);

    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    assert_eq!(scan.stale_removed, 1, "{scan:?}");
    assert!(fs.read(&gone).is_none(), "the stale lease is removed");
    assert_eq!(scan.live.len(), 2, "{scan:?}");
    let by_path = |p: &Path| scan.live.iter().find(|s| s.path == p).unwrap();
    assert_eq!(
        by_path(live.path().unwrap()).lease,
        LiveLease::Record(live.record().unwrap())
    );
    assert!(matches!(
        by_path(torn.path().unwrap()).lease,
        LiveLease::Unreadable(_)
    ));
    let report = scan.judge(5);
    assert_eq!(
        (
            report.live,
            report.current,
            report.unreadable,
            report.stale_removed
        ),
        (2, 1, 1, 1),
        "{report:?}"
    );
    assert!(report.held_back);
    drop(torn);
    let report = scan_leases(&*vfs, Path::new(PACK), true, UUID).judge(5);
    assert_eq!((report.live, report.current), (1, 1), "{report:?}");
    assert!(
        !report.held_back,
        "a live lease at the committed seq lets the sweep run"
    );
}

#[test]
fn a_record_of_another_database_or_lease_id_is_unreadable() {
    let fs = sim();
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let mut foreign = take(&vfs);
    foreign.set_open([9; 16], 5);
    let misnamed = take(&vfs);
    fs.open(misnamed.path().unwrap(), true)
        .unwrap()
        .write_all_at(&LeaseRecord::open(1, 1, UUID, 5).encode(), 0)
        .unwrap();
    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    let whys: Vec<String> = scan
        .live
        .iter()
        .map(|s| match &s.lease {
            LiveLease::Unreadable(why) => why.clone(),
            LiveLease::Record(r) => panic!("read as {r:?}"),
        })
        .collect();
    assert_eq!(whys.len(), 2);
    assert!(
        whys.iter().any(|w| w.contains("another database")),
        "{whys:?}"
    );
    assert!(
        whys.iter().any(|w| w.contains("another lease id")),
        "{whys:?}"
    );
    assert!(scan.judge(5).held_back);
}

fn scanned(seq: Option<u64>) -> ScannedLease {
    let record = seq.map_or_else(
        || LeaseRecord::opening(1, 1),
        |s| LeaseRecord::open(1, 1, UUID, s),
    );
    ScannedLease {
        path: PathBuf::from("/x.lease"),
        lease: LiveLease::Record(record),
    }
}

#[test]
fn only_a_lease_open_at_the_committed_seq_lets_the_sweep_run() {
    let judge = |seq: Option<u64>, committed: u64| {
        let scan = LeaseScan {
            live: vec![scanned(seq)],
            ..LeaseScan::default()
        };
        scan.judge(committed)
    };
    for committed in [0, 7] {
        let r = judge(None, committed);
        assert!(
            r.held_back && r.opening == 1,
            "opening, committed {committed}: {r:?}"
        );
        let r = judge(Some(committed), committed);
        assert!(!r.held_back && r.current == 1, "current {committed}: {r:?}");
        let r = judge(Some(committed + 1), committed);
        assert!(r.held_back && r.ahead == 1, "ahead of {committed}: {r:?}");
    }
    let r = judge(Some(3), 7);
    assert!(
        r.held_back && r.behind == 1 && r.oldest_seq == Some(3),
        "{r:?}"
    );
    assert!(!LeaseScan::default().judge(7).held_back, "no reader lease");
    let unlisted = LeaseScan {
        unlisted: true,
        ..LeaseScan::default()
    };
    assert!(unlisted.judge(7).held_back);
}

#[test]
fn where_locks_cannot_be_judged_nothing_is_opened_or_removed() {
    let fs = sim();
    fs.set_per_process_locks(true);
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let mut live = take(&vfs);
    live.set_open(UUID, 5);
    let left = stale(&fs, 0xbeef);
    let scan = scan_leases(&*vfs, Path::new(PACK), false, UUID);
    assert_eq!((scan.unjudged, scan.stale_removed), (2, 0), "{scan:?}");
    assert!(fs.read(&left).is_some(), "not removed");
    let report = scan.judge(5);
    assert!(report.held_back && report.unreadable == 2, "{report:?}");

    // The pack asks its own file before opening any lease: a writer on such a file system scans this way.
    let pack = pack_support::open_pack(Arc::clone(&vfs), PACK, PackMode::ReadWrite, UUID).unwrap();
    assert_eq!(pack.reader_leases().unwrap(), None, "no pack file yet");
    gc_support::put(&pack, b"a blob", 4, 1);
    let scan = pack.reader_leases().unwrap().unwrap();
    assert_eq!((scan.unjudged, scan.stale_removed), (2, 0), "{scan:?}");
    assert!(fs.read(&left).is_some());
    assert!(fs.read(live.path().unwrap()).is_some());
    let read_only =
        pack_support::open_pack(Arc::clone(&vfs), PACK, PackMode::ReadOnly, UUID).unwrap();
    assert!(matches!(
        read_only.reader_leases(),
        Err(Error::PackReadOnly)
    ));
}

#[test]
fn entries_that_are_not_regular_lease_files_are_left_alone() {
    let fs = sim();
    let dir = leases_dir(Path::new(PACK));
    fs.create_dir(&dir).unwrap();
    let as_dir = dir.join(lease_file_name(5));
    fs.create_dir(&as_dir).unwrap();
    let foreign = dir.join("notes.txt");
    fs.create_new(&foreign).unwrap();
    let scan = scan_leases(&fs, Path::new(PACK), true, UUID);
    assert_eq!(scan, LeaseScan::default());
    assert!(fs.is_dir(&as_dir).unwrap());
    assert!(fs.read(&foreign).is_some());
}

// --- A scripted file system: faults the simulated one does not have ---

#[derive(Debug, Default)]
struct Script {
    /// `try_lock` of a `.lease` file fails (an emulation's `ENOLCK`).
    lock_error: bool,
    /// The next `n` `try_lock`s of a `.lease` file are refused.
    refuse_locks: u32,
    /// The next `n` new `.lease` files are removed right after their creation (the writer judged them stale).
    remove_after_create: u32,
    /// Removed right after a listing of its directory.
    remove_after_list: Option<PathBuf>,
    /// `lock_beside` locks `<path>.lock`, as on Windows.
    sidecars: bool,
    /// The next `n` `lock_beside` fail with `PermissionDenied` (a delete-pending sidecar).
    sidecar_denied: u32,
    /// Creating directories and files fails with this.
    create_fails: Option<io::ErrorKind>,
    /// `create_new` calls of `.lease` files.
    creates: u32,
    /// Left out of every listing (a listing that missed a name).
    hidden: Option<PathBuf>,
    /// The next `n` `same_file` checks of a `.lease` name fail with `PermissionDenied` (a delete-pending name).
    same_file_denied: u32,
    /// `lock_beside` removes the sidecar's name once it opened it (an orphan sweep in between).
    unlink_sidecar: bool,
}

#[derive(Debug, Clone)]
struct Scripted {
    fs: SimFs,
    script: Arc<Mutex<Script>>,
}

#[derive(Debug)]
struct File {
    inner: SimFsFile,
    lease: bool,
    script: Arc<Mutex<Script>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_lease(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with(".lease"))
}

impl Scripted {
    fn new() -> Self {
        Self {
            fs: sim(),
            script: Arc::default(),
        }
    }

    fn vfs(&self) -> Arc<dyn Vfs<File = File>> {
        Arc::new(self.clone())
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        lock(&self.script)
    }

    fn wrap(&self, inner: SimFsFile, path: &Path) -> File {
        File {
            inner,
            lease: is_lease(path),
            script: Arc::clone(&self.script),
        }
    }
}

impl ContainerFile for File {
    fn file_len(&self) -> io::Result<u64> {
        self.inner.file_len()
    }
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.inner.read_exact_at(buf, offset)
    }
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.inner.write_all_at(buf, offset)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
    fn try_lock(&self) -> io::Result<bool> {
        if self.lease {
            let mut s = lock(&self.script);
            if s.lock_error {
                return Err(io::Error::other("ENOLCK"));
            }
            if s.refuse_locks > 0 {
                s.refuse_locks -= 1;
                return Ok(false);
            }
        }
        self.inner.try_lock()
    }
    fn unlock(&self) -> io::Result<()> {
        self.inner.unlock()
    }
    fn locks_per_handle(&self) -> bool {
        self.inner.locks_per_handle()
    }
}

impl Vfs for Scripted {
    type File = File;

    fn open(&self, path: &Path, write: bool) -> io::Result<File> {
        Ok(self.wrap(self.fs.open(path, write)?, path))
    }
    fn create_new(&self, path: &Path) -> io::Result<File> {
        if is_lease(path) {
            let mut s = self.script();
            s.creates += 1;
            if let Some(kind) = s.create_fails {
                return Err(kind.into());
            }
        }
        let file = self.wrap(self.fs.create_new(path)?, path);
        let mut s = self.script();
        if file.lease && s.remove_after_create > 0 {
            s.remove_after_create -= 1;
            self.fs.remove_file(path)?;
        }
        Ok(file)
    }
    fn create_dir(&self, path: &Path) -> io::Result<()> {
        if let Some(kind) = self.script().create_fails {
            return Err(kind.into());
        }
        self.fs.create_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.fs.rename(from, to)
    }
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.fs.hard_link(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.fs.remove_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.fs.sync_dir(dir)
    }
    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        self.fs.entry_kind(path)
    }
    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        self.fs.is_dir(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        let mut listed = self.fs.read_dir(dir)?;
        if let Some(hidden) = &self.script().hidden {
            listed.retain(|e| dir.join(&e.name) != *hidden);
        }
        if let Some(path) = self.script().remove_after_list.take() {
            self.fs.remove_file(&path)?;
        }
        Ok(listed)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.fs.canonicalize(path)
    }
    fn same_file(&self, file: &File, path: &Path) -> io::Result<bool> {
        let mut s = self.script();
        if is_lease(path) && s.same_file_denied > 0 {
            s.same_file_denied -= 1;
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        drop(s);
        self.fs.same_file(&file.inner, path)
    }
    fn lock_beside(&self, path: &Path) -> io::Result<Option<File>> {
        let mut s = self.script();
        if !s.sidecars {
            return Ok(None);
        }
        if s.sidecar_denied > 0 {
            s.sidecar_denied -= 1;
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        drop(s);
        let mut name = path.as_os_str().to_owned();
        name.push(".lock");
        let sidecar = PathBuf::from(name);
        let inner = match self.fs.open(&sidecar, true) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => self.fs.create_new(&sidecar)?,
            Err(e) => return Err(e),
        };
        if self.script().unlink_sidecar {
            self.fs.remove_file(&sidecar)?;
        }
        if inner.try_lock()? {
            Ok(Some(self.wrap(inner, &sidecar)))
        } else {
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
}

#[test]
fn a_lock_error_is_live_and_unreadable_and_a_name_gone_mid_scan_is_skipped() {
    let s = Scripted::new();
    let vfs = s.vfs();
    let mut lease = ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)).unwrap();
    lease.set_open(UUID, 5);
    s.script().lock_error = true;
    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    assert!(
        matches!(&scan.live[..], [ScannedLease { lease: LiveLease::Unreadable(why), .. }] if why.contains("lock")),
        "{scan:?}"
    );
    assert!(scan.judge(5).held_back, "never a GC failure, but held back");
    s.script().lock_error = false;

    let gone = stale(&s.fs, 0xf00d);
    s.script().remove_after_list = Some(gone.clone());
    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    assert_eq!(scan.stale_removed, 0, "{scan:?}");
    assert_eq!(scan.live.len(), 1, "only the held one: {scan:?}");
    assert!(!scan.judge(5).held_back);
}

#[test]
fn a_take_starts_again_when_refused_and_gives_up_after_three_attempts() {
    for (what, refuse, remove) in [("lock refused", 1, 0), ("name removed", 0, 2)] {
        let s = Scripted::new();
        let vfs = s.vfs();
        {
            let mut script = s.script();
            script.refuse_locks = refuse;
            script.remove_after_create = remove;
        }
        let lease = ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)).unwrap();
        assert!(lease.is_held(), "{what}");
        assert_eq!(s.script().creates, 1 + refuse + remove, "{what}");
        assert_eq!(
            lease_files(&s.fs).len(),
            1,
            "{what}: only the held one is left"
        );
    }
    let s = Scripted::new();
    let vfs = s.vfs();
    s.script().refuse_locks = 3;
    assert!(matches!(
        ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)),
        Err(Error::LeaseContended { attempts: 3 })
    ));
    assert!(lease_files(&s.fs).is_empty(), "nothing is left");
}

#[test]
fn with_a_lock_sidecar_the_record_stays_readable_and_stale_sidecars_go() {
    let s = Scripted::new();
    s.script().sidecars = true;
    let vfs = s.vfs();
    let mut lease = ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)).unwrap();
    lease.set_open(UUID, 5);
    let name = lease
        .path()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        lease_files(&s.fs),
        vec![name.clone(), format!("{name}.lock")]
    );
    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    assert_eq!(scan.live.len(), 1, "{scan:?}");
    assert!(
        !scan.judge(5).held_back,
        "read through the unlocked record: {scan:?}"
    );
    drop(lease);
    assert!(lease_files(&s.fs).is_empty(), "both names removed");

    // A stale lease with its sidecar, and a sidecar whose lease is gone.
    let gone = stale(&s.fs, 0xa);
    s.fs.create_new(&PathBuf::from(format!("{}.lock", gone.display())))
        .unwrap();
    let orphan = leases_dir(Path::new(PACK)).join(format!("{}.lock", lease_file_name(0xb)));
    s.fs.create_new(&orphan).unwrap();
    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    assert_eq!(scan.stale_removed, 2, "{scan:?}");
    assert!(lease_files(&s.fs).is_empty(), "{:?}", lease_files(&s.fs));

    // A take racing a stale removal: the sidecar is delete pending, so the take starts again.
    s.script().sidecar_denied = 1;
    let lease = ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)).unwrap();
    assert!(lease.is_held());
    assert_eq!(lease_files(&s.fs).len(), 2, "{:?}", lease_files(&s.fs));
}

#[test]
fn a_directory_that_refuses_writes_gives_a_lease_that_holds_nothing() {
    for kind in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::ReadOnlyFilesystem,
    ] {
        let s = Scripted::new();
        s.script().create_fails = Some(kind);
        let lease = ReaderLease::take_in(&s.vfs(), Path::new(PACK), Path::new(DB)).unwrap();
        assert!(!lease.is_held(), "{kind:?}");
        assert_eq!(lease.path(), None);
        assert_eq!(lease.dir(), leases_dir(Path::new(PACK)));
    }
    // Any other error fails the take.
    let s = Scripted::new();
    s.script().create_fails = Some(io::ErrorKind::StorageFull);
    assert!(matches!(
        ReaderLease::take_in(&s.vfs(), Path::new(PACK), Path::new(DB)),
        Err(Error::PackIo { .. })
    ));
}

/// FIFO, symbolic link and directory entries named like a lease, on the real file system: never opened (a FIFO's
/// open would block), never removed.
#[cfg(unix)]
#[test]
fn special_entries_named_like_a_lease_are_ignored_without_a_hang() {
    let dir = tempfile::tempdir().unwrap();
    let pack = dir.path().join("app.persia.blobs");
    let leases = leases_dir(&pack);
    std::fs::create_dir(&leases).unwrap();
    let fifo = leases.join(lease_file_name(1));
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    let link = leases.join(lease_file_name(2));
    std::os::unix::fs::symlink(dir.path(), &link).unwrap();
    let sub = leases.join(lease_file_name(3));
    std::fs::create_dir(&sub).unwrap();
    let scan = scan_leases(&persia_storage::vfs::RealFs, &pack, true, UUID);
    assert_eq!(scan, LeaseScan::default());
    if made.is_ok_and(|s| s.success()) {
        assert!(fifo.exists());
    }
    assert!(link.symlink_metadata().is_ok() && sub.is_dir());
}

#[test]
fn an_orphan_sidecar_candidate_whose_lease_exists_is_left_alone() {
    // A take in progress creates its lease, then its sidecar; a listing that missed the lease must not make the
    // sweep remove that sidecar.
    let s = Scripted::new();
    s.script().sidecars = true;
    let vfs = s.vfs();
    let lease = stale(&s.fs, 0xc);
    let sidecar = PathBuf::from(format!("{}.lock", lease.display()));
    s.fs.create_new(&sidecar).unwrap();
    s.script().hidden = Some(lease.clone());
    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    assert_eq!(scan.stale_removed, 0, "{scan:?}");
    assert!(
        s.fs.read(&sidecar).is_some(),
        "the sidecar stays while its lease exists"
    );
    // Listed again, the stale lease and its sidecar go together.
    s.script().hidden = None;
    let scan = scan_leases(&*vfs, Path::new(PACK), true, UUID);
    assert_eq!(scan.stale_removed, 1, "{scan:?}");
    assert!(lease_files(&s.fs).is_empty(), "{:?}", lease_files(&s.fs));
}

#[test]
fn a_take_whose_name_or_sidecar_was_removed_before_its_lock_starts_again() {
    // A delete-pending name (Windows): `same_file` is refused, which is "not granted", never a refusing directory.
    let s = Scripted::new();
    s.script().same_file_denied = 1;
    let lease = ReaderLease::take_in(&s.vfs(), Path::new(PACK), Path::new(DB)).unwrap();
    assert!(lease.is_held());
    assert_eq!(s.script().creates, 2);
    assert_eq!(lease_files(&s.fs).len(), 1, "{:?}", lease_files(&s.fs));
    drop(lease);
    // The sidecar's name removed between its open and its lock: the lock protects nothing, so start again (and the
    // attempt removes the lease it created before releasing).
    let s = Scripted::new();
    {
        let mut script = s.script();
        script.sidecars = true;
        script.unlink_sidecar = true;
    }
    assert!(matches!(
        ReaderLease::take_in(&s.vfs(), Path::new(PACK), Path::new(DB)),
        Err(Error::LeaseContended { attempts: 3 })
    ));
    assert!(lease_files(&s.fs).is_empty(), "{:?}", lease_files(&s.fs));
}

#[test]
fn a_stale_lease_is_removed_without_reading_its_record() {
    let fs = sim();
    let dir = leases_dir(Path::new(PACK));
    fs.create_dir(&dir).unwrap();
    let mut v2 = LeaseRecord::open(2, 1, UUID, 3).encode().to_vec();
    v2[4..8].copy_from_slice(&2_u32.to_le_bytes());
    v2.extend([0; 64]);
    for (id, bytes) in [
        (1_u128, b"garbage".to_vec()),
        (2, v2),
        (3, vec![0xAB; 5000]),
    ] {
        let file = fs.create_new(&dir.join(lease_file_name(id))).unwrap();
        file.write_all_at(&bytes, 0).unwrap();
    }
    let scan = scan_leases(&fs, Path::new(PACK), true, UUID);
    assert_eq!((scan.stale_removed, scan.live.len()), (3, 0), "{scan:?}");
    assert!(lease_files(&fs).is_empty());
    // A live one that long is read only up to the bound, and is unreadable.
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let lease = take(&vfs);
    fs.open(lease.path().unwrap(), true)
        .unwrap()
        .write_all_at(&[0; 5000], 64)
        .unwrap();
    let scan = scan_leases(&fs, Path::new(PACK), true, UUID);
    assert!(
        matches!(
            &scan.live[..],
            [ScannedLease {
                lease: LiveLease::Unreadable(_),
                ..
            }]
        ),
        "{scan:?}"
    );
}

#[test]
fn a_leases_path_that_is_a_regular_file_fails_the_take_and_holds_gc_back() {
    let fs = sim();
    fs.create_new(&leases_dir(Path::new(PACK))).unwrap();
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    assert!(matches!(
        ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)),
        Err(Error::PackIo { .. })
    ));
    let scan = scan_leases(&fs, Path::new(PACK), true, UUID);
    assert!(scan.unlisted, "{scan:?}");
    assert!(scan.judge(0).held_back);
}

#[test]
fn a_take_beside_a_file_that_is_not_a_container_creates_nothing() {
    let fs = SimFs::new();
    fs.create_dir(Path::new("/db")).unwrap();
    fs.create_new(Path::new(DB))
        .unwrap()
        .write_all_at(b"not a container", 0)
        .unwrap();
    let vfs: Arc<dyn Vfs<File = SimFsFile>> = Arc::new(fs.clone());
    let e = ReaderLease::take_in(&vfs, Path::new(PACK), Path::new(DB)).unwrap_err();
    assert!(
        matches!(e, Error::Container(_)) && e.is_corruption(),
        "{e:?}"
    );
    assert!(
        fs.read_dir(&leases_dir(Path::new(PACK))).is_err(),
        "nothing created"
    );
}
