//! The local filesystem backend (`file:///…`, SPEC §10.1.1): one file per object under a root directory.
//!
//! Writes go to a temporary file that is fsynced and then renamed (`put`) or hard-linked (`put_if_absent`) to the
//! object's name, and the directory is fsynced before the call returns, so an acknowledged write survives a crash
//! and readers never see a partial object. File names are encoded ([`name`]) so that every valid key round-trips
//! byte for byte, on case-insensitive and normalizing filesystems too.

pub(crate) mod fs;
mod name;

use core::ops::Range;
use std::collections::HashSet;
use std::collections::hash_map::RandomState;
use std::fs::{File, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use bytes::Bytes;
use persia_format::ByteSource;

use crate::error::{Error, Op, Result};
use crate::key::{InvalidKey, Key, KeyPrefix};
use crate::meta::{Capabilities, ObjectMeta, PutOutcome};
use crate::storage::{Storage, check_range, range_source};
use fs::{parent_or_cwd, read_exact_at, sync_dir};

/// Distinguishes the temporary files of one process; [`temp_nonce`] distinguishes processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many temporary names `put` tries before giving up. Names carry a random per-process nonce, so stale files of
/// crashed processes (often all pid 1 in containers) and live processes in other pid namespaces almost never collide.
const TEMP_ATTEMPTS: u32 = 64;

/// How often `put_if_absent` retries when the object it collided with is deleted before it can be read.
const LINK_ATTEMPTS: u32 = 8;

/// Objects stored as files under a root directory (SPEC §10.1.1, ADR-0012).
///
/// Every operation is atomic with respect to other operations, in this process and in others: `put` replaces the
/// file by `rename`, `put_if_absent` publishes it by `link`, which fails if the name exists. An acknowledged write has
/// been fsynced together with every directory on its path.
///
/// ```
/// use bytes::Bytes;
/// use persia_storage::local::LocalStorage;
/// use persia_storage::{Key, KeyPrefix, PutOutcome, Storage};
///
/// # let dir = tempfile::tempdir()?;
/// let storage = LocalStorage::open(dir.path().join("store"))?;
/// let key = Key::new("manifests/00000000000000000001.mf")?;
/// assert_eq!(storage.put_if_absent(&key, Bytes::from_static(b"m1"))?, PutOutcome::Created);
/// assert_eq!(storage.put_if_absent(&key, Bytes::from_static(b"m2"))?, PutOutcome::AlreadyExists);
/// assert_eq!(&*storage.get_range(&key, 1..2)?, b"1");
/// assert_eq!(storage.list(&KeyPrefix::new("manifests/")?)?.len(), 1);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct LocalStorage {
    root: PathBuf,
    /// Directories whose entry in their parent this store has fsynced (SPEC §10.1.1): writing into one needs no
    /// further directory sync on its path.
    synced_dirs: Mutex<HashSet<PathBuf>>,
    /// Directories a read found to be real directories all the way from the root (no symbolic link on the path).
    /// Only another program could change that later, which is outside the contract (SPEC §10.1.1).
    real_dirs: Mutex<HashSet<PathBuf>>,
}

impl LocalStorage {
    /// Opens the store rooted at `root`, creating the directory (and its missing parents, durably) if needed.
    ///
    /// # Errors
    /// The OS error if a directory cannot be created or synced, or if `root` exists and is not a directory.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        // Absolute, so a later change of the working directory cannot move the store.
        let root = std::path::absolute(root.into())?;
        // Create from the outermost missing ancestor inwards, syncing each new directory's parent. Ancestors that
        // already exist are taken as durable, except the root's own entry, synced below (SPEC §10.1.1).
        let missing: Vec<&Path> = root
            .ancestors()
            .filter(|p| !p.as_os_str().is_empty())
            .take_while(|p| !p.exists())
            .collect();
        for dir in missing.iter().rev() {
            match std::fs::create_dir(dir) {
                Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
                _ => sync_dir(parent_or_cwd(dir))?,
            }
        }
        if !std::fs::metadata(&root)?.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a directory", root.display()),
            ));
        }
        // The root's own entry may have been created by another process that has not synced it yet.
        sync_dir(parent_or_cwd(&root))?;
        let synced_dirs = Mutex::new(HashSet::from([root.clone()]));
        let real_dirs = Mutex::new(HashSet::new());
        Ok(Self {
            root,
            synced_dirs,
            real_dirs,
        })
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding `key`'s file, and the file's path.
    fn object_path(&self, key: &Key) -> Result<(PathBuf, PathBuf)> {
        let invalid = |defect| {
            Error::InvalidKey(InvalidKey {
                key: key.as_str().to_owned(),
                defect,
            })
        };
        let mut dir = self.root.clone();
        let mut segments = key.segments().peekable();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                let file = dir.join(name::file_name(segment).map_err(invalid)?);
                return Ok((dir, file));
            }
            dir.push(name::dir_name(segment).map_err(invalid)?);
        }
        // A key has at least one segment.
        Err(invalid(crate::key::KeyDefect::Empty))
    }

    /// Creates `dir` (below the root) if needed and makes its entry, and every entry on its path, durable.
    fn ensure_dir(&self, dir: &Path) -> io::Result<()> {
        if self.is_synced(dir) {
            return Ok(());
        }
        let relative = dir
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::other("object directory outside the store root"))?;
        let mut path = self.root.clone();
        for component in relative.components() {
            path.push(component);
            if self.is_synced(&path) {
                continue;
            }
            match std::fs::create_dir(&path) {
                Ok(()) => {}
                // Not followed if it is a symbolic link: writes never leave the root through a planted link.
                Err(err)
                    if err.kind() == io::ErrorKind::AlreadyExists
                        && std::fs::symlink_metadata(&path)?.is_dir() => {}
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    return Err(io::Error::other(format!(
                        "{} exists and is not a directory",
                        path.display()
                    )));
                }
                Err(err) => return Err(err),
            }
            // Synced whoever created it: another writer may not have synced it yet (SPEC §10.1.1).
            sync_dir(parent_or_cwd(&path))?;
            self.synced_dirs
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(path.clone());
        }
        Ok(())
    }

    fn is_synced(&self, dir: &Path) -> bool {
        // A set insert cannot be left half-done by a panic, so a poisoned lock is safe to keep using.
        self.synced_dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(dir)
    }

    /// Whether `dir` (below the root) and every directory between it and the root is a real directory: reads do not
    /// follow a symbolic link planted on the path, as `list` does not (SPEC §10.1.1). Missing is `false`.
    fn is_real_dir(&self, dir: &Path) -> io::Result<bool> {
        let known = |dir: &Path| {
            self.is_synced(dir)
                || self
                    .real_dirs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains(dir)
        };
        if known(dir) {
            return Ok(true);
        }
        let relative = dir
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::other("object directory outside the store root"))?;
        let mut path = self.root.clone();
        for component in relative.components() {
            path.push(component);
            if known(&path) {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => return Ok(false),
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(false);
                }
                Err(err) => return Err(err),
            }
        }
        self.real_dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(dir.to_path_buf());
        Ok(true)
    }

    /// Opens `key`'s file for reading. A missing file, or an entry `list` would skip (not a regular file, a symbolic
    /// link on the path, a foreign file where a directory of the path should be), is [`Error::NotFound`].
    fn open_object(&self, op: Op, key: &Key, dir: &Path, path: &Path) -> Result<File> {
        match self.is_real_dir(dir) {
            Ok(true) => {}
            Ok(false) => return Err(Error::NotFound { key: key.clone() }),
            Err(err) => return Err(io_error(op, key, err)),
        }
        open_regular(path).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
                Error::NotFound { key: key.clone() }
            }
            _ => io_error(op, key, err),
        })
    }

    /// Forgets every synced directory but the root, after one turned out to be missing (removed by an operator):
    /// the next write recreates and resyncs its path.
    fn forget_synced_dirs(&self) {
        let mut dirs = self
            .synced_dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        dirs.retain(|dir| *dir == self.root);
    }

    /// Writes `data` to a new temporary file in `dir` and fsyncs it.
    fn write_temp(dir: &Path, data: &[u8]) -> io::Result<PathBuf> {
        for _ in 0..TEMP_ATTEMPTS {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            // Starts with `@`, which no encoded segment contains, so `list` never decodes it (SPEC §10.1.1).
            let path = dir.join(format!("@tmp.{:016x}.{n}", temp_nonce()));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            };
            if let Err(err) = file.write_all(data).and_then(|()| file.sync_all()) {
                drop(file);
                return Err(discard_temp(&path, err));
            }
            return Ok(path);
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free temporary file name in {}", dir.display()),
        ))
    }

    /// Writes a synced temporary file for `key`, after creating its directory.
    fn prepare(&self, op: Op, key: &Key, data: &[u8]) -> Result<(PathBuf, PathBuf, PathBuf)> {
        let (dir, path) = self.object_path(key)?;
        let write = || {
            self.ensure_dir(&dir)
                .and_then(|()| Self::write_temp(&dir, data))
        };
        let temp = match write() {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.forget_synced_dirs();
                write()
            }
            written => written,
        }
        .map_err(|err| io_error(op, key, err))?;
        Ok((dir, path, temp))
    }

    /// `put_if_absent` after the temporary file is written; the caller removes it.
    fn link_or_resolve(key: &Key, data: &Bytes, path: &Path, temp: &Path) -> Result<PutOutcome> {
        let io = |err| io_error(Op::PutIfAbsent, key, err);
        for _ in 0..LINK_ATTEMPTS {
            match std::fs::hard_link(temp, path) {
                Ok(()) => return Ok(PutOutcome::Created),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(io(err)),
            }
            // SPEC §10.1: the object found may be this caller's own earlier attempt.
            match read_file(path) {
                Ok(existing) if existing == *data => return Ok(PutOutcome::Created),
                Ok(_) => return Ok(PutOutcome::AlreadyExists),
                // Deleted since the link failed: the name is free again.
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(io(err)),
            }
        }
        Err(io(io::Error::other(
            "the object was replaced and deleted on every attempt",
        )))
    }

    /// Appends every object below `dir` (whose keys start with `base`) to `out`; with `filter`, only names whose
    /// segment starts with it.
    fn collect(
        dir: &Path,
        base: &str,
        filter: Option<&str>,
        out: &mut Vec<ObjectMeta>,
    ) -> io::Result<()> {
        // A deeper tree holds no valid key; stopping here bounds the recursion (one open directory per level, at
        // most ~512) on a hostile tree.
        if base.len() > Key::MAX_LEN {
            return Ok(());
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            let file_name = entry.file_name();
            // A name that is not even UTF-8 was not written by Persia.
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            // Names first: temporary files, which come and go during a list, never reach a `stat`.
            let (encoded, is_dir) = match file_name.strip_suffix(name::DIR_MARKER) {
                Some(encoded) => (encoded, true),
                None => (file_name, false),
            };
            let Some(segment) = name::decode(encoded) else {
                continue;
            };
            if filter.is_some_and(|f| !segment.starts_with(f)) {
                continue;
            }
            // Does not follow symbolic links: Persia never creates them. May `lstat` (filesystems without `d_type`).
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            if (is_dir && !file_type.is_dir()) || (!is_dir && !file_type.is_file()) {
                continue;
            }
            if is_dir {
                Self::collect(&entry.path(), &format!("{base}{segment}/"), None, out)?;
                continue;
            }
            let Ok(key) = Key::new(format!("{base}{segment}")) else {
                continue;
            };
            let size = match entry.metadata() {
                Ok(meta) => meta.len(),
                // Deleted since the directory was read.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            out.push(ObjectMeta { key, size });
        }
        Ok(())
    }
}

impl Storage for LocalStorage {
    fn get_range(&self, key: &Key, range: Range<u64>) -> Result<ByteSource> {
        check_range(key, &range)?;
        let (dir, path) = self.object_path(key)?;
        let io = |err| io_error(Op::GetRange, key, err);
        let file = self.open_object(Op::GetRange, key, &dir, &path)?;
        let len = file.metadata().map_err(io)?.len();
        // Checked before allocating: the range comes from metadata that may be corrupt.
        if range.end > len {
            return Err(Error::ShortRead {
                key: key.clone(),
                returned: len.saturating_sub(range.start),
                requested: range,
            });
        }
        let want = usize::try_from(range.end.saturating_sub(range.start)).map_err(|_| {
            io(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "range larger than the address space",
            ))
        })?;
        let mut buf = vec![0; want];
        match read_exact_at(&file, &mut buf, range.start) {
            Ok(()) => range_source(key, range, Bytes::from(buf)),
            // Truncated since the length check, by something other than Persia (objects are never rewritten).
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                let len = file.metadata().map_err(io)?.len();
                Err(Error::ShortRead {
                    key: key.clone(),
                    returned: len.saturating_sub(range.start),
                    requested: range,
                })
            }
            Err(err) => Err(io(err)),
        }
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        let (dir, path) = self.object_path(key)?;
        let mut file = self.open_object(Op::Get, key, &dir, &path)?;
        // A hint only: the read stops at the end of the file, whatever its length now.
        let len = file.metadata().map_or(0, |meta| meta.len());
        let mut buf = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        file.read_to_end(&mut buf)
            .map_err(|err| io_error(Op::Get, key, err))?;
        Ok(Bytes::from(buf))
    }

    fn put(&self, key: &Key, data: Bytes) -> Result<()> {
        let (dir, path, temp) = self.prepare(Op::Put, key, &data)?;
        let io = |err| io_error(Op::Put, key, err);
        std::fs::rename(&temp, &path).map_err(|err| io(discard_temp(&temp, err)))?;
        sync_dir(&dir).map_err(io)
    }

    fn put_if_absent(&self, key: &Key, data: Bytes) -> Result<PutOutcome> {
        let (dir, path, temp) = self.prepare(Op::PutIfAbsent, key, &data)?;
        let io = |err| io_error(Op::PutIfAbsent, key, err);
        let outcome = match Self::link_or_resolve(key, &data, &path, &temp) {
            Ok(outcome) => outcome,
            Err(err) => {
                // The link error is the one to report; a failed cleanup only leaves a skipped temporary file.
                return Err(match std::fs::remove_file(&temp) {
                    Ok(()) => err,
                    Err(cleanup) => io(io::Error::other(format!(
                        "{err}; removing temporary file {} also failed: {cleanup}",
                        temp.display()
                    ))),
                });
            }
        };
        std::fs::remove_file(&temp).map_err(io)?;
        // For every outcome: an object found may be this caller's earlier attempt or another writer's, linked but not
        // yet synced. What this call reports is durable.
        sync_dir(&dir).map_err(io)?;
        Ok(outcome)
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        // SPEC §10.1: the directory part names a directory; the last part only filters names, never a path.
        let (dir_part, last) = match prefix.as_str().rsplit_once('/') {
            Some((dir_part, last)) => (Some(dir_part), last),
            None => (None, prefix.as_str()),
        };
        let io = |source| Error::Io {
            op: Op::List,
            target: prefix.as_str().to_owned(),
            source,
        };
        // A missing root is an error, not an empty store: an unmounted volume must not look like "no manifests".
        if !std::fs::metadata(&self.root).map_err(io)?.is_dir() {
            return Err(io(io::Error::other("the store root is not a directory")));
        }
        let mut dir = self.root.clone();
        let mut base = String::new();
        if let Some(dir_part) = dir_part {
            for segment in dir_part.split('/') {
                // No object can live under a directory that cannot exist.
                let Ok(name) = name::dir_name(segment) else {
                    return Ok(Vec::new());
                };
                dir.push(name);
                // A file, a symbolic link or nothing where the directory should be: `list("")` skips it, so nothing
                // matches here either (and a link never leads outside the root).
                match std::fs::symlink_metadata(&dir) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => return Ok(Vec::new()),
                    Err(err)
                        if matches!(
                            err.kind(),
                            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                        ) =>
                    {
                        return Ok(Vec::new());
                    }
                    Err(err) => return Err(io(err)),
                }
            }
            base = format!("{dir_part}/");
        }
        let mut out = Vec::new();
        Self::collect(&dir, &base, Some(last), &mut out).map_err(io)?;
        out.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete(&self, key: &Key) -> Result<()> {
        let (dir, path) = self.object_path(key)?;
        let io = |err| io_error(Op::Delete, key, err);
        // Never through a symbolic link on the path: that would remove a file outside the root.
        if !self.is_real_dir(&dir).map_err(io)? {
            return Ok(());
        }
        match std::fs::remove_file(&path) {
            Ok(()) => sync_dir(&dir).map_err(io),
            // Missing, or a file where a directory of the path should be: no such object.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                Ok(())
            }
            Err(err) => Err(io(err)),
        }
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            conditional_put: true,
            strong_list: true,
            append: false,
        }
    }
}

fn io_error(op: Op, key: &Key, source: io::Error) -> Error {
    Error::Io {
        op,
        target: key.as_str().to_owned(),
        source,
    }
}

/// Opens `path` if it is a regular file. Symbolic links and FIFOs are neither followed nor opened (a FIFO would
/// block): Persia never creates them, and `list` skips them (SPEC §10.1.1).
fn open_regular(path: &Path) -> io::Result<File> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "not a regular file",
        ));
    }
    File::open(path)
}

fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    open_regular(path)?.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Removes a temporary file after `err`, and returns `err`, noting a failed removal in it: a leftover temporary file
/// is skipped by `list` and holds no acknowledged data, so the write's own error is the one to report.
fn discard_temp(temp: &Path, err: io::Error) -> io::Error {
    match std::fs::remove_file(temp) {
        Ok(()) => err,
        Err(cleanup) => io::Error::new(
            err.kind(),
            format!(
                "{err}; removing temporary file {} also failed: {cleanup}",
                temp.display()
            ),
        ),
    }
}

/// A random value per process for temporary names: `RandomState` seeds its keys from the OS in each process.
pub(crate) fn temp_nonce() -> u64 {
    static NONCE: OnceLock<u64> = OnceLock::new();
    *NONCE.get_or_init(|| {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.finish()
    })
}
