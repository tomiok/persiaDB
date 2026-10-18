//! An in-memory [`Vfs`] that models what survives a crash, for crash tests of the local backend and the container
//! (ROADMAP 2.4.11).
//!
//! Each directory keeps its entries as readers see them and as its last [`Vfs::sync_dir`] made them durable, plus
//! the changes since, in order (a rename, always within one directory, is one change: old name gone and new name
//! set, or neither). Files are [`SimFile`]s. A crash ([`SimFs::crash`]) keeps the durable entries plus some, all
//! or none of the later changes of each directory (a prefix: directory updates reach the disk in order), and,
//! independently, each file's crash image. So a name can survive while its data does not (a file synced only
//! through its directory), or the data while its name does not (a rename or link whose directory was never synced).
//!
//! One fault schedule ([`SimFault`]) counts every mutating call, of the file system and of its files: after the
//! fault fires, every later mutating call fails, as if the process had died.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{DirEntry, EntryKind, FileType, Vfs};
use crate::container::ContainerFile;
use crate::container::sim::{CrashLoss, Faults, SimFault, SimFile, lock};
use crate::retry::Jitter;

/// What a name points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Node {
    File(u64),
    Dir,
}

/// One change to a directory: names set (`Some`) or removed (`None`), applied together.
type Change = Vec<(OsString, Option<Node>)>;

#[derive(Debug, Default, Clone)]
struct SimDir {
    current: BTreeMap<OsString, Node>,
    durable: BTreeMap<OsString, Node>,
    pending: Vec<Change>,
}

impl SimDir {
    fn apply(&mut self, change: Change) {
        apply(&mut self.current, &change);
        self.pending.push(change);
    }
}

fn apply(entries: &mut BTreeMap<OsString, Node>, change: &Change) {
    for (name, node) in change {
        match node {
            Some(node) => entries.insert(name.clone(), *node),
            None => entries.remove(name),
        };
    }
}

#[derive(Debug)]
struct State {
    /// Every directory, by its normalized path; the root is `/`.
    dirs: BTreeMap<PathBuf, SimDir>,
    files: HashMap<u64, SimFile>,
    next_ino: u64,
    /// Which handle holds each file's writer lock.
    locks: HashMap<u64, u64>,
    /// Locks belong to the process, as `flock` emulated on NFS: every handle of this file system shares them, and closing
    /// any handle of a file releases its lock ([`SimFs::set_per_process_locks`]).
    per_process_locks: bool,
    next_handle: u64,
    faults: Arc<Mutex<Faults>>,
}

/// A simulated file system with crash semantics. Clones share it.
///
/// ```
/// use std::path::Path;
/// use persia_storage::container::sim::CrashLoss;
/// use persia_storage::container::ContainerFile;
/// use persia_storage::vfs::Vfs;
/// use persia_storage::vfs::sim::SimFs;
///
/// let fs = SimFs::new();
/// let f = fs.create_new(Path::new("/a"))?;
/// f.write_all_at(b"data", 0)?;
/// f.sync_data()?; // the data is durable, its name is not
/// assert!(fs.crash(CrashLoss::All).read(Path::new("/a")).is_none());
/// fs.sync_dir(Path::new("/"))?;
/// assert_eq!(fs.crash(CrashLoss::All).read(Path::new("/a")).unwrap(), b"data");
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct SimFs {
    state: Arc<Mutex<State>>,
}

impl Default for SimFs {
    fn default() -> Self {
        Self::new()
    }
}

/// `path` without `.` and `..`, or `None` if it is not absolute.
fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::from("/");
    let mut absolute = false;
    for component in path.components() {
        match component {
            Component::RootDir => absolute = true,
            Component::Prefix(_) | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    absolute.then_some(out)
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{} not found", path.display()),
    )
}

impl State {
    /// The directory and name of `path`, which must be absolute and not the root.
    fn split(path: &Path) -> io::Result<(PathBuf, OsString)> {
        let path = normalize(path)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "relative path"))?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
            .to_owned();
        let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        Ok((dir, name))
    }

    fn lookup(&self, path: &Path) -> io::Result<Node> {
        if normalize(path).is_some_and(|p| p == Path::new("/")) {
            return Ok(Node::Dir);
        }
        let (dir, name) = Self::split(path)?;
        self.dirs
            .get(&dir)
            .and_then(|d| d.current.get(&name).copied())
            .ok_or_else(|| not_found(path))
    }

    fn dir_mut(&mut self, dir: &Path) -> io::Result<&mut SimDir> {
        self.dirs.get_mut(dir).ok_or_else(|| not_found(dir))
    }

    fn file(&self, ino: u64) -> io::Result<SimFile> {
        self.files
            .get(&ino)
            .cloned()
            .ok_or_else(|| io::Error::other("dangling inode"))
    }

    /// Counts a mutating call (see [`Faults::begin`]).
    fn begin(&self) -> io::Result<bool> {
        lock(&self.faults).begin()
    }

    fn handle(
        &mut self,
        ino: u64,
        writable: bool,
        state: &Arc<Mutex<State>>,
    ) -> io::Result<SimFsFile> {
        let handle = self.next_handle;
        self.next_handle = handle.saturating_add(1);
        Ok(SimFsFile {
            file: self.file(ino)?,
            ino,
            handle,
            writable,
            state: Arc::clone(state),
        })
    }
}

impl SimFs {
    /// An empty file system: only the root directory `/`, durable.
    pub fn new() -> Self {
        let mut dirs = BTreeMap::new();
        dirs.insert(PathBuf::from("/"), SimDir::default());
        Self {
            state: Arc::new(Mutex::new(State {
                dirs,
                files: HashMap::new(),
                next_ino: 1,
                locks: HashMap::new(),
                per_process_locks: false,
                next_handle: 1,
                faults: Arc::default(),
            })),
        }
    }

    /// Arms (or with `None` disarms) a fault, counting mutating calls of the file system and all its files since it
    /// was created (or crashed into existence).
    pub fn set_fault(&self, fault: Option<SimFault>) {
        lock(&lock(&self.state).faults).set(fault);
    }

    /// Makes locks belong to the process rather than the handle, as `flock` emulated with POSIX locks on Linux NFS and
    /// some FUSE mounts: every handle's [`ContainerFile::try_lock`] succeeds, closing any handle of a file releases its
    /// lock, and [`ContainerFile::locks_per_handle`] is `false` (ADR-0019).
    pub fn set_per_process_locks(&self, on: bool) {
        lock(&self.state).per_process_locks = on;
    }

    /// How many mutating calls have been made: the `n` of the next one.
    pub fn mutations(&self) -> u64 {
        lock(&lock(&self.state).faults).mutations()
    }

    /// Whether a fault has fired.
    pub fn crashed(&self) -> bool {
        lock(&lock(&self.state).faults).crashed()
    }

    /// The current contents of the file at `path`, or `None` if there is none.
    pub fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let state = lock(&self.state);
        match state.lookup(path).ok()? {
            Node::File(ino) => state.files.get(&ino).map(SimFile::bytes),
            Node::Dir => None,
        }
    }

    /// Every file, as `(path, contents)`, sorted by path.
    pub fn files(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let state = lock(&self.state);
        let mut out = Vec::new();
        for (dir, d) in &state.dirs {
            for (name, node) in &d.current {
                if let Node::File(ino) = node
                    && let Some(f) = state.files.get(ino)
                {
                    out.push((dir.join(name), f.bytes()));
                }
            }
        }
        out.sort();
        out
    }

    /// The file system a restart finds after a crash now: new and independent, everything durable, no fault armed.
    #[must_use]
    pub fn crash(&self, loss: CrashLoss) -> Self {
        let state = lock(&self.state);
        let rng = match loss {
            CrashLoss::Seeded(seed) => Some(Jitter::new(seed)),
            CrashLoss::All | CrashLoss::Nothing => None,
        };
        let new = Self::new();
        let mut out = lock(&new.state);
        let faults = Arc::clone(&out.faults);
        let mut inodes: HashMap<u64, u64> = HashMap::new();
        let mut stack = vec![PathBuf::from("/")];
        while let Some(path) = stack.pop() {
            let Some(dir) = state.dirs.get(&path) else {
                continue;
            };
            let all = dir.pending.len();
            let keep = match (loss, &rng) {
                (CrashLoss::Nothing, _) => all,
                (CrashLoss::Seeded(_), Some(rng)) => {
                    let n = rng
                        .next_u64()
                        .checked_rem((all as u64).saturating_add(1))
                        .unwrap_or(0);
                    usize::try_from(n).unwrap_or(0)
                }
                _ => 0,
            };
            let mut entries = dir.durable.clone();
            for change in dir.pending.iter().take(keep) {
                apply(&mut entries, change);
            }
            let mut crashed = SimDir::default();
            for (name, node) in entries {
                let node = match node {
                    Node::Dir => {
                        stack.push(path.join(&name));
                        Node::Dir
                    }
                    Node::File(ino) => {
                        let new_ino = if let Some(&n) = inodes.get(&ino) {
                            n
                        } else {
                            let image = state.files.get(&ino).map_or_else(Vec::new, |f| {
                                let file_loss = match loss {
                                    CrashLoss::Seeded(seed) => CrashLoss::Seeded(
                                        seed ^ ino.wrapping_mul(0x9e37_79b9_7f4a_7c15),
                                    ),
                                    other => other,
                                };
                                f.crash_image(file_loss)
                            });
                            let n = out.next_ino;
                            out.next_ino = n.saturating_add(1);
                            out.files
                                .insert(n, SimFile::with_faults(image, Arc::clone(&faults)));
                            inodes.insert(ino, n);
                            n
                        };
                        Node::File(new_ino)
                    }
                };
                crashed.current.insert(name, node);
            }
            crashed.durable = crashed.current.clone();
            out.dirs.insert(path, crashed);
        }
        drop(out);
        new
    }

    /// Runs a mutating directory operation: counts it, applies the fault, then `f`.
    fn mutate<T>(&self, f: impl FnOnce(&mut State) -> io::Result<T>) -> io::Result<T> {
        let (ok, value) = {
            let mut state = lock(&self.state);
            let ok = state.begin()?;
            (ok, f(&mut state)?)
        };
        // After the state lock is released: dropping a `SimFsFile` here takes it.
        Faults::finish(ok).map(|()| value)
    }
}

impl Vfs for SimFs {
    type File = SimFsFile;

    fn open(&self, path: &Path, write: bool) -> io::Result<SimFsFile> {
        let mut state = lock(&self.state);
        match state.lookup(path)? {
            Node::File(ino) => state.handle(ino, write, &self.state),
            Node::Dir => Err(io::ErrorKind::IsADirectory.into()),
        }
    }

    fn create_new(&self, path: &Path) -> io::Result<SimFsFile> {
        self.mutate(|state| {
            let (dir, name) = State::split(path)?;
            let ino = state.next_ino;
            let faults = Arc::clone(&state.faults);
            let d = state.dir_mut(&dir)?;
            if d.current.contains_key(&name) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            d.apply(vec![(name, Some(Node::File(ino)))]);
            state.next_ino = ino.saturating_add(1);
            state
                .files
                .insert(ino, SimFile::with_faults(Vec::new(), faults));
            state.handle(ino, true, &self.state)
        })
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.mutate(|state| {
            let (dir, name) = State::split(path)?;
            let full = dir.join(&name);
            let d = state.dir_mut(&dir)?;
            if d.current.contains_key(&name) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            d.apply(vec![(name, Some(Node::Dir))]);
            // A directory whose entry was lost and is created again starts empty (its old contents are unreachable).
            state.dirs.insert(full, SimDir::default());
            Ok(())
        })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.mutate(|state| {
            let Node::File(ino) = state.lookup(from)? else {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "SimFs renames files only",
                ));
            };
            if state.lookup(to).is_ok_and(|n| n == Node::Dir) {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            let (from_dir, from_name) = State::split(from)?;
            let (to_dir, to_name) = State::split(to)?;
            if from_dir != to_dir {
                // Persia renames only within a directory (temporary file next to its target). Modeling a crash-atomic
                // rename across two directories' pending changes is not worth it until something needs it.
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "SimFs renames within one directory only",
                ));
            }
            // One change: on a crash the rename happened or did not.
            state
                .dir_mut(&from_dir)?
                .apply(vec![(from_name, None), (to_name, Some(Node::File(ino)))]);
            Ok(())
        })
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.mutate(|state| {
            let Node::File(ino) = state.lookup(from)? else {
                return Err(io::ErrorKind::IsADirectory.into());
            };
            let (dir, name) = State::split(to)?;
            let d = state.dir_mut(&dir)?;
            if d.current.contains_key(&name) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            d.apply(vec![(name, Some(Node::File(ino)))]);
            Ok(())
        })
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.mutate(|state| {
            if state.lookup(path)? == Node::Dir {
                return Err(io::ErrorKind::IsADirectory.into());
            }
            let (dir, name) = State::split(path)?;
            state.dir_mut(&dir)?.apply(vec![(name, None)]);
            Ok(())
        })
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.mutate(|state| {
            let dir = normalize(dir).ok_or_else(|| not_found(dir))?;
            let d = state.dir_mut(&dir)?;
            d.durable = d.current.clone();
            d.pending.clear();
            Ok(())
        })
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        let state = lock(&self.state);
        Ok(match state.lookup(path)? {
            Node::Dir => EntryKind::Dir,
            Node::File(ino) => EntryKind::File {
                len: state.file(ino)?.file_len()?,
            },
        })
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        Ok(lock(&self.state).lookup(path)? == Node::Dir)
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        let state = lock(&self.state);
        let path = normalize(dir).ok_or_else(|| not_found(dir))?;
        let d = state.dirs.get(&path).ok_or_else(|| not_found(dir))?;
        if state.lookup(&path)? != Node::Dir {
            return Err(not_found(dir));
        }
        d.current
            .iter()
            .map(|(name, node)| {
                Ok(DirEntry {
                    name: name.clone(),
                    kind: match node {
                        Node::Dir => FileType::Dir,
                        Node::File(_) => FileType::File,
                    },
                })
            })
            .collect()
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let state = lock(&self.state);
        state.lookup(path)?;
        normalize(path).ok_or_else(|| not_found(path))
    }

    fn same_file(&self, file: &SimFsFile, path: &Path) -> io::Result<bool> {
        match lock(&self.state).lookup(path) {
            Ok(node) => Ok(node == Node::File(file.ino)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    fn lock_beside(&self, _path: &Path) -> io::Result<Option<SimFsFile>> {
        Ok(None)
    }
}

/// An open file of a [`SimFs`]: its writer lock ([`ContainerFile::try_lock`]) excludes other handles of the same
/// file until this one is dropped.
#[derive(Debug)]
pub struct SimFsFile {
    file: SimFile,
    ino: u64,
    handle: u64,
    /// Opened for writing: writes through a read-only handle fail, as on a real file.
    writable: bool,
    state: Arc<Mutex<State>>,
}

impl SimFsFile {
    fn check_writable(&self) -> io::Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the file was opened read-only",
            ))
        }
    }
}

impl SimFsFile {
    /// The underlying file.
    pub fn file(&self) -> &SimFile {
        &self.file
    }
}

impl ContainerFile for SimFsFile {
    fn file_len(&self) -> io::Result<u64> {
        self.file.file_len()
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        self.check_writable()?;
        self.file.write_all_at(buf, offset)
    }

    fn sync_data(&self) -> io::Result<()> {
        self.file.sync_data()
    }

    fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.check_writable()?;
        self.file.set_len(len)
    }

    fn try_lock(&self) -> io::Result<bool> {
        let mut state = lock(&self.state);
        let per_process = state.per_process_locks;
        let holder = state.locks.entry(self.ino).or_insert(self.handle);
        Ok(per_process || *holder == self.handle)
    }

    fn locks_per_handle(&self) -> bool {
        !lock(&self.state).per_process_locks
    }
}

impl Drop for SimFsFile {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        if state.per_process_locks || state.locks.get(&self.ino) == Some(&self.handle) {
            state.locks.remove(&self.ino);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> &Path {
        Path::new(s)
    }

    #[test]
    fn names_are_durable_only_after_a_directory_sync() {
        let fs = SimFs::new();
        fs.create_dir(p("/d")).unwrap();
        let f = fs.create_new(p("/d/a")).unwrap();
        f.write_all_at(b"x", 0).unwrap();
        f.sync_data().unwrap();
        // `/d` itself was never synced into `/`.
        fs.sync_dir(p("/d")).unwrap();
        assert!(fs.crash(CrashLoss::All).read(p("/d/a")).is_none());
        fs.sync_dir(p("/")).unwrap();
        assert_eq!(fs.crash(CrashLoss::All).read(p("/d/a")).unwrap(), b"x");
        assert_eq!(fs.crash(CrashLoss::Nothing).files().len(), 1);
    }

    #[test]
    fn a_name_can_survive_without_its_data() {
        let fs = SimFs::new();
        let f = fs.create_new(p("/a")).unwrap();
        f.write_all_at(b"unsynced", 0).unwrap();
        fs.sync_dir(p("/")).unwrap();
        assert_eq!(fs.crash(CrashLoss::All).read(p("/a")).unwrap(), b"");
    }

    #[test]
    fn a_rename_survives_whole_or_not_at_all() {
        let fs = SimFs::new();
        for (name, data) in [("/old", b"old"), ("/new", b"new")] {
            let f = fs.create_new(p(name)).unwrap();
            f.write_all_at(data, 0).unwrap();
            f.sync_data().unwrap();
        }
        fs.sync_dir(p("/")).unwrap();
        fs.rename(p("/new"), p("/old")).unwrap();
        let mut seen = std::collections::HashSet::new();
        for seed in 0..32 {
            let c = fs.crash(CrashLoss::Seeded(seed));
            let state = (c.read(p("/old")).unwrap(), c.read(p("/new")).is_some());
            assert!(
                state == (b"old".to_vec(), true) || state == (b"new".to_vec(), false),
                "{state:?}"
            );
            seen.insert(state);
        }
        assert_eq!(seen.len(), 2, "both outcomes occur");
        assert!(fs.rename(p("/missing"), p("/x")).is_err());
        fs.create_dir(p("/d")).unwrap();
        assert_eq!(
            fs.rename(p("/old"), p("/d/old")).unwrap_err().kind(),
            io::ErrorKind::Unsupported
        );
    }

    #[test]
    fn hard_links_share_the_file_across_a_crash() {
        let fs = SimFs::new();
        let f = fs.create_new(p("/t")).unwrap();
        fs.hard_link(p("/t"), p("/a")).unwrap();
        assert_eq!(
            fs.hard_link(p("/t"), p("/a")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        fs.remove_file(p("/t")).unwrap();
        f.write_all_at(b"data", 0).unwrap();
        f.sync_data().unwrap();
        fs.sync_dir(p("/")).unwrap();
        let c = fs.crash(CrashLoss::All);
        assert_eq!(c.files(), [(PathBuf::from("/a"), b"data".to_vec())]);
    }

    #[test]
    fn one_fault_schedule_covers_directories_and_files() {
        let fs = SimFs::new();
        let f = fs.create_new(p("/a")).unwrap(); // call 0
        assert_eq!(fs.mutations(), 1);
        fs.set_fault(Some(SimFault::FailAt(2)));
        f.write_all_at(b"x", 0).unwrap(); // call 1
        assert!(fs.sync_dir(p("/")).is_err()); // call 2 fails
        assert!(fs.crashed());
        assert!(f.sync_data().is_err(), "the process is gone");
        assert!(fs.create_new(p("/b")).is_err());
        assert_eq!(fs.crash(CrashLoss::All).files(), []);
        assert!(!fs.crash(CrashLoss::All).crashed());
    }

    #[test]
    fn locks_exclude_other_handles_until_dropped() {
        let fs = SimFs::new();
        let a = fs.create_new(p("/a")).unwrap();
        let b = fs.open(p("/a"), true).unwrap();
        assert!(a.try_lock().unwrap());
        assert!(a.try_lock().unwrap(), "a holder may ask again");
        assert!(!b.try_lock().unwrap());
        drop(a);
        assert!(b.try_lock().unwrap());
        assert!(fs.same_file(&b, p("/a")).unwrap());
        assert!(!fs.same_file(&b, p("/nope")).unwrap());
    }

    #[test]
    fn errors_match_the_os() {
        let fs = SimFs::new();
        assert_eq!(
            fs.open(p("/x"), false).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            fs.create_new(p("/no/x")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        fs.create_dir(p("/d")).unwrap();
        assert_eq!(
            fs.create_dir(p("/d")).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            fs.remove_file(p("/d")).unwrap_err().kind(),
            io::ErrorKind::IsADirectory
        );
        assert_eq!(
            fs.remove_file(p("/x")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(fs.entry_kind(p("/d")).unwrap(), EntryKind::Dir);
        assert!(fs.is_dir(p("/d/../d")).unwrap());
        assert_eq!(fs.canonicalize(p("/d/./")).unwrap(), PathBuf::from("/d"));
        assert_eq!(fs.read_dir(p("/d")).unwrap(), []);
        assert_eq!(fs.read_dir(p("/")).unwrap().len(), 1);
    }
}
