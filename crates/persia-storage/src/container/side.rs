//! The files beside a container (SPEC §4.5.1, §4.6, §9.3; ADR-0014): the temporary files of creates and compactions a
//! crash interrupted, the quarantine copies of corrupt tails, and those of its blob pack (`<name>.blobs`): the
//! temporary files of its creations and rewrites, its writer lock file and its reader lease directory. Listed by path;
//! removed through a read-write handle, whose writer lock keeps a create, compaction or recovery of this container from
//! adding one meanwhile, and, for the pack's temporary files, under the pack's own writer lock.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::{Container, ContainerFile, Error, Result};
use crate::local::fs::parent_or_cwd;
use crate::side::{self, InUse, SideFile, SideFileKind};
use crate::vfs::{FileType, RealFs, Vfs};

/// What the blob pack's file name adds to its container's (SPEC §9.3): `persia-blob`'s `PACK_SUFFIX`, which sits
/// above this crate.
const PACK_SUFFIX: &str = ".blobs";

/// What a name beside the container is, as found by [`scan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// One of the container's own: [`SideFileKind::Temporary`] or [`SideFileKind::Quarantine`].
    Container(SideFileKind),
    /// `<container>.blobs.tmp.<16 lowercase hex>.<n>`: a pack creation's or rewrite's temporary file.
    PackTemporary,
    /// `<container>.blobs.lock`: the pack's writer lock (ROADMAP 9.2.15). `regular`: a regular file, as a pack writer
    /// creates it. Anything else there (a symbolic link, a directory) is never opened here: a pack writer follows a
    /// link, so whether one holds the lock cannot be told without following it.
    PackLock { regular: bool },
    /// `<container>.blobs.leases`: the pack's reader lease directory (ROADMAP 9.2.16).
    PackLeases,
}

impl Side {
    const fn kind(self) -> SideFileKind {
        match self {
            Self::Container(kind) => kind,
            Self::PackTemporary => SideFileKind::Temporary,
            Self::PackLock { .. } => SideFileKind::Lock,
            Self::PackLeases => SideFileKind::Leases,
        }
    }
}

/// What `name` is beside the container named `container`: a temporary file of a create or compaction,
/// `<container>.tmp.<16 hex>.<n>` or `<container>.compact.<16 hex>.<n>`, or a quarantine copy,
/// `<container>.quarantine.<16 hex>.<16 hex>.<n>`; `None` for anything else.
fn kind_of(name: &str, container: &str) -> Option<SideFileKind> {
    let rest = name.strip_prefix(container)?;
    if let Some(rest) = rest.strip_prefix(".quarantine.") {
        let (from, rest) = rest.split_once('.')?;
        return (is_hex16(from) && is_nonce_and_counter(rest)).then_some(SideFileKind::Quarantine);
    }
    let rest = rest
        .strip_prefix(".tmp.")
        .or_else(|| rest.strip_prefix(".compact."))?;
    is_nonce_and_counter(rest).then_some(SideFileKind::Temporary)
}

/// What the entry `name` of type `file_type` is beside the container named `container`: one of its own names
/// ([`kind_of`], regular files only) or one of its blob pack's (ROADMAP 9.2.10). The pack's temporary names are
/// matched as strictly as the pack's own open-time sweep matches them (regular files, lowercase hex only, the exact
// name the pack makes); its lock is listed whatever it is (a pack writer follows a link there), and its lease
/// directory only as a directory.
fn side_of(name: &str, file_type: FileType, container: &str) -> Option<Side> {
    let pack = name
        .strip_prefix(container)
        .and_then(|rest| rest.strip_prefix(PACK_SUFFIX));
    match (pack, file_type) {
        (Some(".lock"), kind) => Some(Side::PackLock {
            regular: kind == FileType::File,
        }),
        (Some(".leases"), FileType::Dir) => Some(Side::PackLeases),
        (Some(rest), FileType::File) => rest
            .strip_prefix(".tmp.")
            .is_some_and(is_lowercase_nonce_and_counter)
            .then_some(Side::PackTemporary),
        (None, FileType::File) => kind_of(name, container).map(Side::Container),
        _ => None,
    }
}

/// `<16 digits or lowercase hex letters>.<decimal counter>`: the tail of the names the blob pack's `temp_path` makes.
fn is_lowercase_nonce_and_counter(s: &str) -> bool {
    let lowercase = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
    s.split_once('.').is_some_and(|(nonce, n)| {
        nonce.len() == 16
            && nonce.bytes().all(lowercase)
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
    })
}

/// `<16 hex digits>.<decimal counter>`, the tail of every name [`super::path::temp_path`] makes.
fn is_nonce_and_counter(s: &str) -> bool {
    s.split_once('.').is_some_and(|(nonce, n)| {
        is_hex16(nonce) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
    })
}

fn is_hex16(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The side files of the container at `path` on `vfs`: `(directory, [(path, side)])`, sorted by path.
fn scan<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
) -> Result<(PathBuf, Vec<(PathBuf, Side)>)> {
    let dir = parent_or_cwd(path).to_owned();
    let Some(container) = path.file_name().and_then(|n| n.to_str()) else {
        // Limitation: side names are matched as UTF-8, so those of a container whose name is not UTF-8 are not
        // found.
        return Ok((dir, Vec::new()));
    };
    let mut found = Vec::new();
    for entry in vfs.read_dir(&dir).map_err(Error::io("list directory"))? {
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        if let Some(side) = side_of(name, entry.kind, container) {
            found.push((dir.join(name), side));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((dir, found))
}

/// The pack beside the container at `path`: `<name>.blobs`.
fn pack_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(PACK_SUFFIX);
    PathBuf::from(name)
}

/// The blob pack's writer lock beside the pack at `pack` (SPEC §9.3, ROADMAP 9.2.15), taken for a moment as a pack
/// writer takes it: on Windows through [`Vfs::lock_beside`], elsewhere on `<pack>.lock` with
/// [`ContainerFile::try_lock`]. Whether a pack writer holds it and, if this call took it, the locked file, to release
/// with [`release_pack_lock`]. `found` is what [`scan`] saw at `<pack>.lock`: `None`, nothing (a pack writer creates the
/// file before it creates or rewrites anything, and nothing removes it, so without `create` that is [`InUse::No`]);
/// `Some(true)`, a regular file; `Some(false)`, anything else, which is never opened ([`InUse::Unknown`]: a pack
/// writer follows a link there, so only following it would tell, and the target may be anything).
///
/// While this call holds the lock, a pack writer opening at that instant fails with its `PackLocked` and may retry.
fn take_pack_lock<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    pack: &Path,
    found: Option<bool>,
    create: bool,
) -> Result<(InUse, Option<F>)> {
    match found {
        Some(false) => return Ok((InUse::Unknown, None)),
        None if !create => return Ok((InUse::No, None)),
        _ => {}
    }
    let mut lock = pack.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    match vfs.lock_beside(pack) {
        // A lock file's lock is the handle's on Windows (`LockFileEx`).
        Ok(Some(file)) => return Ok((InUse::No, Some(file))),
        Ok(None) => {}
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok((InUse::Yes, None)),
        Err(e) => return Err(Error::io("lock the pack's lock file")(e)),
    }
    let opened = if create {
        match vfs.create_new(&lock) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => vfs.open(&lock, true),
            other => other,
        }
    } else {
        vfs.open(&lock, false)
    };
    let file = match opened {
        Ok(file) => file,
        // Removed since the scan (not by Persia): no pack writer can hold it.
        Err(e) if !create && e.kind() == io::ErrorKind::NotFound => return Ok((InUse::No, None)),
        Err(e) => return Err(Error::io("open the pack's lock file")(e)),
    };
    let (in_use, took) = side::in_use(&file).map_err(Error::io("lock the pack's lock file"))?;
    Ok((in_use, took.then_some(file)))
}

/// What [`scan`] found at the pack's lock path: `None`, nothing; `Some(regular)`, an entry.
fn pack_lock_found(found: &[(PathBuf, Side)]) -> Option<bool> {
    found.iter().find_map(|(_, side)| match side {
        Side::PackLock { regular } => Some(*regular),
        _ => None,
    })
}

/// Releases a pack lock [`take_pack_lock`] took, then returns `result`.
fn release_pack_lock<F: ContainerFile, T>(held: Option<F>, result: io::Result<T>) -> io::Result<T> {
    match held {
        Some(file) => side::unlock_kept(&file, result),
        None => result,
    }
}

impl Container<File> {
    /// [`Container::side_files_in`] on the operating system's file system.
    ///
    /// # Errors
    /// See [`Container::side_files_in`].
    pub fn side_files(path: impl AsRef<Path>) -> Result<Vec<SideFile>> {
        Self::side_files_in(&RealFs, path)
    }
}

impl<F: ContainerFile + 'static> Container<F> {
    /// The temporary and quarantine files next to the container at `path` (which need not exist), and those of its
    /// blob pack `<name>.blobs` (SPEC §9.3; ROADMAP 9.2.10): the temporary files of its creations and rewrites, its
    /// writer lock file and its reader lease directory. Sorted by path. Removes none: [`Container::sweep_stale_files`]
    /// and [`Container::remove_quarantine_files`] do, and nothing removes the lock or the lease directory.
    ///
    /// A container's temporary file is in use while its create or compaction holds its lock; it is found by taking
    /// that lock for a moment, which a create racing with this call may find held, and fail with [`Error::Locked`]. On
    /// Windows files are not locked (SPEC §4.5.1), so whether one is in use is [`InUse::Unknown`]. A quarantine copy is
    /// never in use outside a recovery, which holds the container's writer lock. The pack's lock file and temporary
    /// files are in use while a pack writer holds the pack's writer lock (ROADMAP 9.2.15), found by taking it for a
    /// moment (on Windows too); with no lock file, no pack writer has run since it would have been created. A pack
    /// writer opening at that moment fails with `PackLocked`: retry it. A lock path that is not a regular file (a
    /// symbolic link, which a pack writer follows) is listed but never opened, and it and the pack's temporary files
    /// are [`InUse::Unknown`] (the sweep keeps them). Whether the lease directory is in use is never checked
    /// ([`InUse::Unknown`]).
    ///
    /// ```
    /// use persia_storage::container::Container;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// drop(Container::create(&path)?);
    /// std::fs::write(dir.path().join("app.persia.compact.0123456789abcdef.0"), b"a crashed compaction")?;
    /// std::fs::write(dir.path().join("app.persia.blobs.tmp.0123456789abcdef.0"), b"a crashed pack rewrite")?;
    /// let files = Container::side_files(&path)?;
    /// assert_eq!(files.len(), 2);
    /// assert!(!files[0].removed);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::Io`] listing the directory or opening or locking a file.
    pub fn side_files_in(vfs: &dyn Vfs<File = F>, path: impl AsRef<Path>) -> Result<Vec<SideFile>> {
        let path = path.as_ref();
        // A container behind a symbolic link has its side files next to its target, as `open_in` finds it.
        let path = match vfs.canonicalize(path) {
            Ok(path) => path,
            // No container there (yet): its side files would still be next to the path.
            Err(err) if err.kind() == io::ErrorKind::NotFound => path.to_owned(),
            Err(err) => return Err(Error::io("canonicalize")(err)),
        };
        let (_, found) = scan(vfs, &path)?;
        let lock = pack_lock_found(&found);
        let pack_files = found
            .iter()
            .any(|(_, side)| matches!(side, Side::PackTemporary | Side::PackLock { .. }));
        let pack_in_use = if pack_files {
            let (in_use, held) = take_pack_lock(vfs, &pack_path(&path), lock, false)?;
            release_pack_lock(held, Ok(in_use)).map_err(Error::io("unlock the pack's lock file"))?
        } else {
            InUse::No
        };
        let mut files = Vec::new();
        for (path, side) in found {
            let in_use = match side {
                Side::Container(SideFileKind::Temporary) => match vfs.open(&path, false) {
                    Ok(file) => match side::in_use(&file).map_err(Error::io("lock"))? {
                        (in_use, true) => {
                            side::unlock_kept(&file, Ok(in_use)).map_err(Error::io("unlock"))?
                        }
                        (in_use, false) => in_use,
                    },
                    // Finished meanwhile: renamed or removed by its writer.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(Error::io("open temporary file")(err)),
                },
                Side::Container(_) => InUse::No,
                Side::PackTemporary | Side::PackLock { .. } => pack_in_use,
                Side::PackLeases => InUse::Unknown,
            };
            files.push(SideFile {
                path,
                kind: side.kind(),
                in_use,
                removed: false,
            });
        }
        Ok(files)
    }

    /// Removes the temporary files that a crashed [`Container::create_in`] or [`Container::compact`] of this container
    /// left next to it (`<name>.tmp.*`, `<name>.compact.*`; SPEC §4.5.1, ROADMAP 2.4.15), and those of crashed
    /// creations or rewrites of its blob pack (`<name>.blobs.tmp.*`; SPEC §9.3, ROADMAP 9.2.10), and returns their
    /// paths. They never hold acknowledged data. Quarantine files (ADR-0014) are kept: they are for an operator. The
    /// pack's lock file and lease directory are never removed.
    ///
    /// Needs a read-write handle opened by path: holding the writer lock, no compaction of this container can be in
    /// flight, and a file whose own writer lock is still held (a create racing with this open, which will fail with
    /// [`Error::AlreadyExists`]) is skipped. A create that has made its temporary file but not yet locked it may lose
    /// it here and then fail with an I/O error instead. The pack's temporary files are removed only while this call
    /// holds the pack's writer lock (creating its file if missing), as a pack writer's open removes them: while a pack
    /// writer holds it (a handle that outlived its database), or where it proves nothing (Windows aside, a lock that
    /// belongs to the process, ADR-0019, or a lock path that is not a regular file), they are all kept. A pack writer
    /// opening while this call holds the lock fails with `PackLocked`: retry it. One directory sync covers every
    /// removal.
    ///
    /// # Errors
    /// [`Error::ReadOnly`], [`Error::Poisoned`], [`Error::InvalidInput`] without a path, or [`Error::Io`] listing the
    /// directory, taking the pack's lock, removing a file or syncing the directory (files removed before the error
    /// stay removed).
    pub fn sweep_stale_files(&self) -> Result<Vec<PathBuf>> {
        self.remove_side_files(SideFileKind::Temporary, None)
    }

    /// Removes the given quarantine copies of this container (ADR-0014), as [`Container::side_files`] listed them,
    /// and returns the paths it removed; those already gone are skipped. **A quarantine copy may hold acknowledged
    /// writes** that recovery could not prove stale: pass only copies an operator has inspected or saved (`persia
    /// container sweep --quarantine` passes those listed before it opened the container, never one its own open
    /// made; ROADMAP 2.4.19).
    ///
    /// Needs a read-write handle opened by path: holding the writer lock, no recovery of this container can be adding
    /// one meanwhile.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if a path is not a quarantine file next to this container (nothing is removed then),
    /// otherwise as [`Container::sweep_stale_files`].
    pub fn remove_quarantine_files(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let location = self.location.as_ref().ok_or_else(|| {
            Error::InvalidInput("the container was not opened from a path".into())
        })?;
        let dir = parent_or_cwd(&location.path);
        let container = location.path.file_name().and_then(|n| n.to_str());
        for path in paths {
            let name = path.file_name().and_then(|n| n.to_str());
            let ours = path.parent() == Some(dir)
                && name.zip(container).and_then(|(n, c)| kind_of(n, c))
                    == Some(SideFileKind::Quarantine);
            if !ours {
                return Err(Error::InvalidInput(format!(
                    "{} is not a quarantine file of {}",
                    path.display(),
                    location.path.display()
                )));
            }
        }
        self.remove_side_files(SideFileKind::Quarantine, Some(paths))
    }

    /// Removes the side files of `kind` (only those in `only`, if given) that no writer holds.
    fn remove_side_files(
        &self,
        kind: SideFileKind,
        only: Option<&[PathBuf]>,
    ) -> Result<Vec<PathBuf>> {
        self.writable()?;
        let location = self.location.as_ref().ok_or_else(|| {
            Error::InvalidInput("the container was not opened from a path".into())
        })?;
        let vfs = &*location.vfs;
        let (dir, found) = scan(vfs, &location.path)?;
        let mut removed = Vec::new();
        let wanted = found.iter().filter(|(path, side)| {
            *side == Side::Container(kind) && only.is_none_or(|only| only.contains(path))
        });
        let mut result = Ok(());
        for (path, _) in wanted {
            match remove_unless_held(vfs, path) {
                Ok(true) => removed.push(path.clone()),
                Ok(false) => {}
                Err(err) => {
                    result = Err(err);
                    break;
                }
            }
        }
        let pack_temps: Vec<&PathBuf> = found
            .iter()
            .filter(|(_, side)| kind == SideFileKind::Temporary && *side == Side::PackTemporary)
            .map(|(path, _)| path)
            .collect();
        // The pack's under its writer lock, held through the directory sync (ROADMAP 9.2.10, 9.2.15).
        let mut pack_lock = None;
        if result.is_ok() && !pack_temps.is_empty() {
            match take_pack_lock(
                vfs,
                &pack_path(&location.path),
                pack_lock_found(&found),
                true,
            ) {
                Ok((InUse::No, Some(held))) => {
                    pack_lock = Some(held);
                    for path in pack_temps {
                        match vfs.remove_file(path) {
                            Ok(()) => removed.push(path.clone()),
                            // Gone meanwhile.
                            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                            Err(err) => {
                                result = Err(Error::io("remove side file")(err));
                                break;
                            }
                        }
                    }
                }
                // A pack writer lives, or the lock proves nothing here: every pack temporary file is kept.
                Ok((_, held)) => {
                    result = release_pack_lock(held, Ok(()))
                        .map_err(Error::io("unlock the pack's lock file"));
                }
                Err(err) => result = Err(err),
            }
        }
        // Even after an error: what was removed stays removed across a crash.
        let synced = if removed.is_empty() {
            Ok(())
        } else {
            vfs.sync_dir(&dir)
        };
        let released = release_pack_lock(pack_lock, Ok(()));
        let outcome = match (result, synced) {
            (Ok(()), Ok(())) => Ok(removed),
            (Ok(()), Err(sync)) => Err(Error::io("sync directory")(sync)),
            // The removal's error is the one to report, the failed sync is added to it.
            (Err(Error::Io { op, source }), Err(sync)) => Err(Error::Io {
                op,
                source: side::then_failed(source, "syncing the directory", sync),
            }),
            // `remove_unless_held` returns only `Error::Io`; were it to return another error, it is kept as the source.
            (Err(err), Err(sync)) => Err(Error::Io {
                op: "remove side file",
                source: side::then_failed(io::Error::other(err), "syncing the directory", sync),
            }),
            (Err(err), Ok(())) => Err(err),
        };
        // A failed release of the pack's lock is the error, or is added to the one before it.
        match (outcome, released) {
            (outcome, Ok(())) => outcome,
            (Ok(_), Err(unlock)) => Err(Error::io("unlock the pack's lock file")(unlock)),
            (Err(Error::Io { op, source }), Err(unlock)) => Err(Error::Io {
                op,
                source: side::then_failed(source, "unlocking the pack's writer lock", unlock),
            }),
            (Err(err), Err(unlock)) => Err(Error::Io {
                op: "remove side file",
                source: side::then_failed(
                    io::Error::other(err),
                    "unlocking the pack's writer lock",
                    unlock,
                ),
            }),
        }
    }
}

/// Removes the side file at `path` unless a live writer holds its lock, holding the lock across the removal (ADR-0019):
/// whether it removed it.
fn remove_unless_held<F: ContainerFile>(vfs: &dyn Vfs<File = F>, path: &Path) -> Result<bool> {
    let file = match vfs.open(path, false) {
        Ok(file) => file,
        // Gone meanwhile: its creator removed it.
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(Error::io("open side file")(err)),
    };
    // A temporary file held by a live create. On Windows files are not locked (the lock is beside the container): no
    // check, and the writer lock this handle holds excludes compactions.
    // Where locks belong to the process (NFS, FUSE; ADR-0019) a lock taken proves nothing: a create in this process
    // may hold it, so the file is kept.
    // A file kept with its lock taken is unlocked before the close, which a forked copy of the descriptor would
    // outlive (SPEC §4.5.1, ADR-0019 decision 7).
    let locked = match side::try_lock(&file).map_err(Error::io("lock"))? {
        Some(false) => return Ok(false),
        Some(true) if !file.locks_per_handle() => {
            return side::unlock_kept(&file, Ok(false)).map_err(Error::io("unlock"));
        }
        Some(true) => true,
        None => false,
    };
    let failed = match vfs.remove_file(path) {
        // The name is gone (removed here or by its creator): no one can meet the lock.
        Ok(()) => return Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => err,
    };
    let failed = if locked {
        side::unlock_kept(&file, Err(failed))
    } else {
        Err(failed)
    };
    drop(file);
    failed.map_err(Error::io("remove side file"))
}

#[cfg(test)]
mod tests {
    use super::kind_of;
    use crate::side::SideFileKind::{Quarantine, Temporary};

    #[test]
    fn only_this_containers_side_names_are_found() {
        let c = "app.persia";
        assert_eq!(
            kind_of("app.persia.tmp.0123456789abcdef.0", c),
            Some(Temporary)
        );
        assert_eq!(
            kind_of("app.persia.compact.0123456789ABCDEF.42", c),
            Some(Temporary)
        );
        assert_eq!(
            kind_of(
                "app.persia.quarantine.0000000000003028.0123456789abcdef.1",
                c
            ),
            Some(Quarantine)
        );
        for name in [
            "app.persia",
            "app.persia.lock",
            "app.persia.quarantine.0000000000003028.1",
            "app.persia.quarantine.000000000000302.0123456789abcdef.1",
            "app.persia.quarantine.0000000000003028.0123456789abcdef.",
            "app.persia.tmp.0123456789abcdef",
            "app.persia.tmp.0123456789abcde.1",
            "app.persia.tmp.0123456789abcdeg.1",
            "app.persia.tmp.0123456789abcdef.1x",
            "app.persia.tmp.0123456789abcdef.",
            "other.persia.tmp.0123456789abcdef.1",
            "app.persia.blobs.tmp.0123456789abcdef.1",
        ] {
            assert_eq!(kind_of(name, c), None, "{name}");
        }
    }
}

#[cfg(test)]
mod pack_tests {
    use super::{Side, side_of};
    use crate::side::SideFileKind::Temporary;
    use crate::vfs::FileType::{Dir, File, Other};

    /// ROADMAP 9.2.10: the pack's temporary names as strictly as its own sweep matches them, its lock a regular file
    /// and its lease directory a directory; nothing else of the pack's.
    #[test]
    fn only_the_packs_own_names_are_its_side_files() {
        let c = "app.persia";
        assert_eq!(
            side_of("app.persia.blobs.tmp.0123456789abcdef.0", File, c),
            Some(Side::PackTemporary)
        );
        assert_eq!(
            side_of("app.persia.blobs.lock", File, c),
            Some(Side::PackLock { regular: true })
        );
        // Listed whatever it is, but only a regular file is ever opened.
        for kind in [Dir, Other] {
            assert_eq!(
                side_of("app.persia.blobs.lock", kind, c),
                Some(Side::PackLock { regular: false }),
                "{kind:?}"
            );
        }
        assert_eq!(
            side_of("app.persia.blobs.leases", Dir, c),
            Some(Side::PackLeases)
        );
        assert_eq!(
            side_of("app.persia.tmp.0123456789abcdef.0", File, c),
            Some(Side::Container(Temporary))
        );
        for (name, kind) in [
            ("app.persia.blobs", File),
            ("app.persia.blobs.tmp.0123456789ABCDEF.1", File),
            ("app.persia.blobs.tmp.0123456789abcdef.0", Dir),
            ("app.persia.blobs.tmp.0123456789abcdef.0", Other),
            ("app.persia.blobs.tmp.0123456789abcdef", File),
            ("app.persia.blobs.tmp.0123456789abcde.1", File),
            ("app.persia.blobs.tmp.0123456789abcdef0.1", File),
            ("app.persia.blobs.tmp.0123456789abcdef.", File),
            ("app.persia.blobs.tmp.0123456789abcdef.1x", File),
            ("app.persia.blobs.lock.1", File),
            ("app.persia.blobs.leases", File),
            ("app.persia.blobs.leases", Other),
            ("app.persia.blobs.leases.tmp", Dir),
            ("app.persia.tmp.0123456789abcdef.0", Dir),
            ("other.persia.blobs.lock", File),
            ("other.persia.blobs.tmp.0123456789abcdef.0", File),
        ] {
            assert_eq!(side_of(name, kind, c), None, "{name} {kind:?}");
        }
    }
}
