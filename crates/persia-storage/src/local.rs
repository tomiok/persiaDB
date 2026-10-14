//! The local filesystem backend (`file:///…`, SPEC §10.1.1): one file per object under a root directory.
//!
//! Writes go to a temporary file that is fsynced and then renamed (`put`) or hard-linked (`put_if_absent`) to the
//! object's name, and the directory is fsynced before the call returns, so an acknowledged write survives a crash
//! and readers never see a partial object. File names are encoded ([`name`]) so that every valid key round-trips
//! byte for byte, on case-insensitive and normalizing filesystems too.

mod fs;
mod name;

use core::ops::Range;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use bytes::Bytes;
use persia_format::ByteSource;

use crate::error::{Error, Op, Result};
use crate::key::{InvalidKey, Key, KeyPrefix};
use crate::meta::{Capabilities, ObjectMeta, PutOutcome};
use crate::storage::{Storage, check_range, range_source};
use fs::{read_exact_at, sync_dir};

/// Distinguishes the temporary files of one process; the pid distinguishes processes.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How many temporary names `put` tries before giving up: only stale files from a crashed process with the same pid
/// can be in the way.
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
}

impl LocalStorage {
    /// Opens the store rooted at `root`, creating the directory (and its missing parents, durably) if needed.
    ///
    /// # Errors
    /// The OS error if a directory cannot be created or synced, or if `root` exists and is not a directory.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        // Create from the outermost missing ancestor inwards, syncing each new directory's parent.
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
        Ok(Self { root, synced_dirs })
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
                Err(err) if err.kind() != io::ErrorKind::AlreadyExists => return Err(err),
                // Synced whoever created it: another writer may not have synced it yet (SPEC §10.1.1).
                _ => sync_dir(parent_or_cwd(&path))?,
            }
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

    /// Writes `data` to a new temporary file in `dir` and fsyncs it.
    fn write_temp(dir: &Path, data: &[u8]) -> io::Result<PathBuf> {
        for _ in 0..TEMP_ATTEMPTS {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            // Starts with the directory marker, so no object name is ever a temporary name (SPEC §10.1.1).
            let path = dir.join(format!(
                "{}tmp.{}.{n}",
                name::DIR_MARKER,
                std::process::id()
            ));
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
        let temp = self
            .ensure_dir(&dir)
            .and_then(|()| Self::write_temp(&dir, data))
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
        // A deeper tree holds no valid key; stopping here bounds the recursion on a hostile tree.
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
            // Does not follow symbolic links: Persia never creates them.
            let file_type = entry.file_type()?;
            let (encoded, is_dir) = match file_name.strip_suffix(name::DIR_MARKER) {
                Some(encoded) if file_type.is_dir() => (encoded, true),
                None if file_type.is_file() => (file_name, false),
                _ => continue,
            };
            let Some(segment) = name::decode(encoded) else {
                continue;
            };
            if filter.is_some_and(|f| !segment.starts_with(f)) {
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
        let (_, path) = self.object_path(key)?;
        let io = |err| io_error(Op::GetRange, key, err);
        let file = open_object(Op::GetRange, key, &path)?;
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
        read_exact_at(&file, &mut buf, range.start).map_err(io)?;
        range_source(key, range, Bytes::from(buf))
    }

    fn get(&self, key: &Key) -> Result<Bytes> {
        let (_, path) = self.object_path(key)?;
        let mut file = open_object(Op::Get, key, &path)?;
        let mut buf = Vec::new();
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
        // Also for an equal object found: it may be this caller's earlier attempt, linked but not yet synced.
        if outcome == PutOutcome::Created {
            sync_dir(&dir).map_err(io)?;
        }
        Ok(outcome)
    }

    fn list(&self, prefix: &KeyPrefix) -> Result<Vec<ObjectMeta>> {
        // SPEC §10.1: the directory part names a directory; the last part only filters names, never a path.
        let (dir_part, last) = match prefix.as_str().rsplit_once('/') {
            Some((dir_part, last)) => (Some(dir_part), last),
            None => (None, prefix.as_str()),
        };
        let mut dir = self.root.clone();
        let mut base = String::new();
        if let Some(dir_part) = dir_part {
            for segment in dir_part.split('/') {
                match name::dir_name(segment) {
                    Ok(name) => dir.push(name),
                    // No object can live under a directory that cannot exist.
                    Err(_) => return Ok(Vec::new()),
                }
            }
            base = format!("{dir_part}/");
        }
        let mut out = Vec::new();
        Self::collect(&dir, &base, Some(last), &mut out).map_err(|source| Error::Io {
            op: Op::List,
            target: prefix.as_str().to_owned(),
            source,
        })?;
        out.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    fn delete(&self, key: &Key) -> Result<()> {
        let (dir, path) = self.object_path(key)?;
        let io = |err| io_error(Op::Delete, key, err);
        match std::fs::remove_file(&path) {
            Ok(()) => sync_dir(&dir).map_err(io),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
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

/// Opens `key`'s file for reading; a missing file is [`Error::NotFound`].
fn open_object(op: Op, key: &Key, path: &Path) -> Result<File> {
    File::open(path).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => Error::NotFound { key: key.clone() },
        _ => io_error(op, key, err),
    })
}

fn read_file(path: &Path) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    File::open(path)?.read_to_end(&mut buf)?;
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

/// The directory to sync for an entry of `path`: its parent, or the current directory for a relative single name.
fn parent_or_cwd(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}
