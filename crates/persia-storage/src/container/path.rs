//! The container by path: create, open and compact through a [`Vfs`] (SPEC §4.5.1, §4.6), so crash tests can run
//! them on a simulated file system (ROADMAP 2.4.11).

use std::fs::File;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use persia_format::block::Compression;

use super::{
    Container, ContainerFile, Error, LockedBy, Mode, Relocation, Repair, Result, TailPolicy,
    new_db_uuid, take_lock,
};
use crate::local::fs::parent_or_cwd;
use crate::local::temp_nonce;
use crate::vfs::{EntryKind, RealFs, Vfs};

/// Distinguishes the temporary files of one process's creates and compactions.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// How often [`Container::open`] reopens a path that a compaction renamed over while it was taking the lock.
const OPEN_ATTEMPTS: u32 = 8;

/// Where a container opened by path lives, and the file system it lives on.
#[derive(Debug)]
pub(super) struct Location<F> {
    pub(super) vfs: Arc<dyn Vfs<File = F>>,
    /// The canonical path found at create or open; compaction renames over it.
    pub(super) path: PathBuf,
    /// The writer lock held beside the container ([`Vfs::lock_beside`], Windows), if any.
    pub(super) lock: Option<F>,
}

/// `<dir>/<name><suffix>.<nonce>.<n>`, a fresh temporary name next to `path`.
pub(super) fn temp_path(path: &Path, suffix: &str) -> Result<PathBuf> {
    let mut name = path
        .file_name()
        .ok_or_else(|| Error::InvalidInput(format!("{} has no file name", path.display())))?
        .to_owned();
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    name.push(format!("{suffix}.{:016x}.{n}", temp_nonce()));
    Ok(parent_or_cwd(path).join(name))
}

/// The writer lock beside `path`, if the platform uses one.
fn lock_beside<F: ContainerFile>(vfs: &dyn Vfs<File = F>, path: &Path) -> Result<Option<F>> {
    vfs.lock_beside(path).map_err(|err| match err.kind() {
        io::ErrorKind::WouldBlock => Error::Locked,
        _ => Error::io("lock")(err),
    })
}

impl Container<File> {
    /// [`Container::create_in`] on the operating system's file system.
    ///
    /// # Errors
    /// See [`Container::create_in`].
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        Self::create_in(Arc::new(RealFs), path)
    }

    /// [`Container::open_in`] on the operating system's file system.
    ///
    /// # Errors
    /// See [`Container::open_in`].
    pub fn open(path: impl AsRef<Path>, mode: Mode) -> Result<Self> {
        Self::open_in(Arc::new(RealFs), path, mode)
    }

    /// [`Container::open_in_with`] on the operating system's file system.
    ///
    /// # Errors
    /// See [`Container::open_in_with`].
    pub fn open_with(path: impl AsRef<Path>, mode: Mode, repair: Repair) -> Result<Self> {
        Self::open_in_with(Arc::new(RealFs), path, mode, repair)
    }
}

impl<F: ContainerFile + 'static> Container<F> {
    /// Creates a new container at `path` on `vfs` and opens it read-write, holding the writer lock.
    ///
    /// Never truncates or replaces a file (SPEC §4.7): the container is written to a temporary file in the same
    /// directory, synced, and hard-linked to `path`, which fails if the name exists; then the temporary name is
    /// removed and the directory synced. A crash leaves no container, or a complete one, at `path` (and possibly a
    /// stale temporary file, `<name>.tmp.<nonce>.<n>`). The file system must support hard links.
    ///
    /// # Errors
    /// [`Error::AlreadyExists`] if `path` exists, or [`Error::Io`]. An `Io` error from removing the temporary name
    /// or syncing the directory comes after the link: the container then exists at `path`, unlocked, and a retry
    /// gets `AlreadyExists`.
    pub fn create_in(vfs: Arc<dyn Vfs<File = F>>, path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let dir = parent_or_cwd(path);
        let lock = match lock_beside(&*vfs, path) {
            Err(Error::Locked) if vfs.entry_kind(path).is_ok() => {
                return Err(Error::AlreadyExists {
                    path: path.to_owned(),
                });
            }
            other => other?,
        };
        let temp = temp_path(path, ".tmp")?;
        let file = vfs.create_new(&temp).map_err(Error::io("create"))?;
        let locked_by = if lock.is_some() {
            LockedBy::Beside
        } else {
            LockedBy::File
        };
        let result =
            Self::create_locked_by(file, new_db_uuid(), locked_by).and_then(|container| match vfs
                .hard_link(&temp, path)
            {
                Ok(()) => Ok(container),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    Err(Error::AlreadyExists {
                        path: path.to_owned(),
                    })
                }
                Err(err) => Err(Error::io("link")(err)),
            });
        // The temporary name goes whatever happened. One directory sync then covers the link and the removal, and
        // runs even if the removal failed, so a container reported or not is durable either way.
        let removed = vfs
            .remove_file(&temp)
            .map_err(Error::io("remove temporary file"));
        let synced = vfs.sync_dir(dir).map_err(Error::io("sync directory"));
        let mut container = result?;
        removed?;
        synced?;
        container.location = Some(Location {
            path: vfs.canonicalize(path).map_err(Error::io("canonicalize"))?,
            vfs,
            lock,
        });
        Ok(container)
    }

    /// Opens the container at `path` on `vfs` and recovers it (SPEC §4.4, §8.6). [`Mode::ReadWrite`] takes the
    /// writer lock: on the container file (`flock`, Unix), or on `<name>.lock` beside it (Windows, SPEC §4.5.1).
    ///
    /// # Errors
    /// [`Error::Locked`] if another handle holds the writer lock, and the errors of [`Container::open_on`].
    pub fn open_in(
        vfs: Arc<dyn Vfs<File = F>>,
        path: impl AsRef<Path>,
        mode: Mode,
    ) -> Result<Self> {
        Self::open_in_with(vfs, path, mode, Repair::Safe)
    }

    /// [`Container::open_in`], choosing what a read-write open may do when the frames after the last commit end in
    /// corruption (SPEC §4.5.1, ADR-0014): with [`Repair::Safe`] (what `open_in` does), zeros and stale frames are
    /// truncated, other bytes are first copied to `<name>.quarantine.<offset>.<nonce>.<n>` next to the container
    /// ([`Recovery::quarantined`](super::Recovery::quarantined)), and frames that may hold acknowledged writes make
    /// the open fail with [`Error::CorruptTail`]; [`Repair::Quarantine`] quarantines those too.
    ///
    /// # Errors
    /// As [`Container::open_in`].
    pub fn open_in_with(
        vfs: Arc<dyn Vfs<File = F>>,
        path: impl AsRef<Path>,
        mode: Mode,
        repair: Repair,
    ) -> Result<Self> {
        // Canonical, so compaction renames over the real file, not over a symbolic link to it.
        let path = vfs.canonicalize(path.as_ref()).map_err(Error::io("open"))?;
        let mut lock = match mode {
            Mode::ReadWrite => lock_beside(&*vfs, &path)?,
            Mode::ReadOnly => None,
        };
        for _ in 0..OPEN_ATTEMPTS {
            let file = vfs
                .open(&path, mode == Mode::ReadWrite)
                .map_err(Error::io("open"))?;
            // With a lock beside the container (Windows), the path cannot change under it: it is never renamed.
            if mode == Mode::ReadWrite && lock.is_none() {
                take_lock(&file)?;
                // A compaction may have renamed a new file over the path between our open and our lock; the lock
                // we got is then on the old, unlinked file, which its writer has just released. Open again.
                if !vfs.same_file(&file, &path).map_err(Error::io("stat"))? {
                    continue;
                }
            }
            let policy = TailPolicy {
                quarantine: Some((&*vfs, &path)),
                repair,
            };
            let mut container = Self::recover(file, mode, policy)?;
            container.location = Some(Location {
                vfs,
                path,
                lock: lock.take(),
            });
            return Ok(container);
        }
        // The path changed under every attempt: a writer is busy compacting.
        Err(Error::Locked)
    }

    /// The path this container was created or opened at, if it was.
    pub fn path(&self) -> Option<&Path> {
        self.location.as_ref().map(|l| l.path.as_path())
    }

    /// Compacts the container in place (SPEC §4.6): [`Container::compact_into`] a temporary file
    /// `<name>.compact.<nonce>.<n>` next to it, then renames that over the container and syncs the directory. This
    /// handle then works on the new file; frame offsets the caller held are replaced by the returned
    /// [`Relocation`]. Handles that opened the old file keep reading it.
    ///
    /// The path names a complete old or new container at every instant. On an error before the rename, the
    /// temporary file is removed and this handle is as [`Container::compact_into`] left it. The rename goes to the
    /// canonical path found at open (a symbolic link to the container keeps pointing at it; other hard links to the
    /// file are not supported: they keep the old contents), and is refused if that path no longer names this file
    /// (not checked on Windows, which exposes no file identity). If the directory sync fails after the rename,
    /// [`Error::CompactedNotDurable`] carries the [`Relocation`] and this handle moves to the new file, poisoned.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if the container was not opened from a path or the path names another file,
    /// [`Error::CompactedNotDurable`], and the errors of [`Container::compact_into`].
    pub fn compact(
        &mut self,
        live: &[Range<u64>],
        manifest: impl FnOnce(&Relocation) -> Vec<u8>,
        compression: Compression,
    ) -> Result<Relocation> {
        let (vfs, path) = match &self.location {
            Some(l) => (Arc::clone(&l.vfs), l.path.clone()),
            None => {
                return Err(Error::InvalidInput(
                    "the container was not opened from a path".into(),
                ));
            }
        };
        self.writable()?;
        let dir = parent_or_cwd(&path).to_owned();
        let temp = temp_path(&path, ".compact")?;
        let file = vfs
            .create_new(&temp)
            .map_err(Error::io("create compaction file"))?;
        let discard = |err: Error| match vfs.remove_file(&temp) {
            Ok(()) => err,
            Err(cleanup) => Error::Io {
                op: "compact",
                source: io::Error::other(format!(
                    "{err}; removing {} also failed: {cleanup}",
                    temp.display()
                )),
            },
        };
        let locked_by = match self.location.as_ref().and_then(|l| l.lock.as_ref()) {
            Some(_) => LockedBy::Beside,
            None => LockedBy::File,
        };
        let (mut new, relocation) =
            match self.compact_locked_by(file, live, manifest, compression, locked_by) {
                Ok(done) => done,
                Err(err) => return Err(discard(err)),
            };
        // Never rename over a path that no longer names this file (replaced from outside): that file is not ours.
        let renamed = match vfs.same_file(&self.file, &path) {
            Ok(true) => vfs.rename(&temp, &path).map_err(Error::io("rename")),
            Ok(false) => Err(Error::InvalidInput(format!(
                "{} no longer names this container",
                path.display()
            ))),
            Err(err) => Err(Error::io("stat")(err)),
        };
        if let Err(err) = renamed {
            drop(new);
            return Err(discard(err));
        }
        let synced = vfs.sync_dir(&dir);
        new.location = Some(Location {
            vfs,
            path,
            lock: self.location.as_mut().and_then(|l| l.lock.take()),
        });
        new.poisoned = synced.is_err();
        *self = new;
        match synced {
            Ok(()) => Ok(relocation),
            Err(source) => Err(Error::CompactedNotDurable { relocation, source }),
        }
    }
}

/// Whether `name` is a temporary file of a create or compaction of the container named `container`:
/// `<container>.tmp.<16 hex digits>.<n>` or `<container>.compact.<16 hex digits>.<n>` (SPEC §4.5.1, §4.6).
fn is_stale_candidate(name: &str, container: &str) -> bool {
    let Some(rest) = name.strip_prefix(container) else {
        return false;
    };
    let Some(rest) = rest
        .strip_prefix(".tmp.")
        .or_else(|| rest.strip_prefix(".compact."))
    else {
        return false;
    };
    let Some((nonce, n)) = rest.split_once('.') else {
        return false;
    };
    nonce.len() == 16
        && nonce.bytes().all(|b| b.is_ascii_hexdigit())
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
}

impl<F: ContainerFile + 'static> Container<F> {
    /// Removes the temporary files that a crashed [`Container::create_in`] or [`Container::compact`] of this container
    /// left next to it (`<name>.tmp.*`, `<name>.compact.*`; SPEC §4.5.1, ROADMAP 2.4.15), and returns their paths.
    /// They never hold acknowledged data. Quarantine files (ADR-0014) are kept: they are for an operator.
    ///
    /// Needs a read-write handle opened by path: holding the writer lock, no compaction of this container can be in
    /// flight, and a file whose own writer lock is still held (a create racing with this open, which will fail with
    /// [`Error::AlreadyExists`]) is skipped. A create that has made its temporary file but not yet locked it may lose
    /// it here and then fail with an I/O error instead.
    ///
    /// # Errors
    /// [`Error::ReadOnly`], [`Error::Poisoned`], [`Error::InvalidInput`] without a path, or [`Error::Io`] listing the
    /// directory, removing a file or syncing the directory (files removed before the error stay removed).
    pub fn sweep_stale_files(&self) -> Result<Vec<PathBuf>> {
        self.writable()?;
        let location = self.location.as_ref().ok_or_else(|| {
            Error::InvalidInput("the container was not opened from a path".into())
        })?;
        let vfs = &*location.vfs;
        let dir = parent_or_cwd(&location.path);
        let Some(container) = location.path.file_name().and_then(|n| n.to_str()) else {
            // A name that is not UTF-8 never got temporary files from this code.
            return Ok(Vec::new());
        };
        let mut removed = Vec::new();
        for entry in vfs.read_dir(dir).map_err(Error::io("list directory"))? {
            let Some(name) = entry.name.to_str() else {
                continue;
            };
            if !matches!(entry.kind, EntryKind::File { .. }) || !is_stale_candidate(name, container)
            {
                continue;
            }
            let path = dir.join(name);
            let file = match vfs.open(&path, false) {
                Ok(file) => file,
                // Gone meanwhile: its creator removed it.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::io("open temporary file")(err)),
            };
            // Held by a live create. On Windows files are not locked (the lock is beside the container): no check.
            match file.try_lock() {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) if err.kind() == io::ErrorKind::Unsupported => {}
                Err(err) => return Err(Error::io("lock")(err)),
            }
            drop(file);
            match vfs.remove_file(&path) {
                Ok(()) => removed.push(path),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::io("remove temporary file")(err)),
            }
        }
        if !removed.is_empty() {
            vfs.sync_dir(dir).map_err(Error::io("sync directory"))?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::is_stale_candidate;

    #[test]
    fn only_this_containers_temporary_names_are_candidates() {
        let c = "app.persia";
        assert!(is_stale_candidate("app.persia.tmp.0123456789abcdef.0", c));
        assert!(is_stale_candidate(
            "app.persia.compact.0123456789ABCDEF.42",
            c
        ));
        for name in [
            "app.persia",
            "app.persia.lock",
            "app.persia.quarantine.0000000000003028.0123456789abcdef.1",
            "app.persia.tmp.0123456789abcdef",
            "app.persia.tmp.0123456789abcde.1",
            "app.persia.tmp.0123456789abcdeg.1",
            "app.persia.tmp.0123456789abcdef.1x",
            "app.persia.tmp.0123456789abcdef.",
            "other.persia.tmp.0123456789abcdef.1",
            "app.persia.blobs.tmp.0123456789abcdef.1",
        ] {
            assert!(!is_stale_candidate(name, c), "{name}");
        }
    }
}
