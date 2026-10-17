//! The files beside a container (SPEC §4.5.1, §4.6; ADR-0014): the temporary files of creates and compactions a
//! crash interrupted, and the quarantine copies of corrupt tails. Listed by path; removed through a read-write handle,
//! whose writer lock keeps a create, compaction or recovery of this container from adding one meanwhile.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use super::{Container, ContainerFile, Error, Result};
use crate::local::fs::parent_or_cwd;
use crate::side::{self, InUse, SideFile, SideFileKind};
use crate::vfs::{FileType, RealFs, Vfs};

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

/// `<16 hex digits>.<decimal counter>`, the tail of every name [`super::path::temp_path`] makes.
fn is_nonce_and_counter(s: &str) -> bool {
    s.split_once('.').is_some_and(|(nonce, n)| {
        is_hex16(nonce) && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
    })
}

fn is_hex16(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The side files of the container at `path` on `vfs`: `(directory, [(path, kind)])`.
fn scan<F: ContainerFile>(
    vfs: &dyn Vfs<File = F>,
    path: &Path,
) -> Result<(PathBuf, Vec<(PathBuf, SideFileKind)>)> {
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
        if entry.kind != FileType::File {
            continue;
        }
        if let Some(kind) = kind_of(name, container) {
            found.push((dir.join(name), kind));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((dir, found))
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
    /// The temporary and quarantine files next to the container at `path` (which need not exist), sorted by path.
    /// Removes none: [`Container::sweep_stale_files`] and [`Container::remove_quarantine_files`] do.
    ///
    /// A temporary file is in use while its create or compaction holds its lock; it is found by taking that lock for
    /// a moment, which a create racing with this call may find held, and fail with [`Error::Locked`]. On Windows files
    /// are not locked (SPEC §4.5.1), so whether one is in use is [`InUse::Unknown`]. A quarantine copy is never in use
    /// outside a recovery, which holds the container's writer lock.
    ///
    /// ```
    /// use persia_storage::container::Container;
    ///
    /// # let dir = tempfile::tempdir()?;
    /// let path = dir.path().join("app.persia");
    /// drop(Container::create(&path)?);
    /// std::fs::write(dir.path().join("app.persia.compact.0123456789abcdef.0"), b"a crashed compaction")?;
    /// let files = Container::side_files(&path)?;
    /// assert_eq!(files.len(), 1);
    /// assert!(!files[0].removed);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    ///
    /// # Errors
    /// [`Error::Io`] listing the directory or opening or locking a file.
    pub fn side_files_in(vfs: &dyn Vfs<File = F>, path: impl AsRef<Path>) -> Result<Vec<SideFile>> {
        let path = path.as_ref();
        // A container behind a symbolic link has its side files next to its target, as `open_in` finds it.
        let path = vfs.canonicalize(path).unwrap_or_else(|_| path.to_owned());
        let (_, found) = scan(vfs, &path)?;
        let mut files = Vec::new();
        for (path, kind) in found {
            let in_use = match kind {
                SideFileKind::Quarantine => InUse::No,
                SideFileKind::Temporary => match vfs.open(&path, false) {
                    Ok(file) => InUse::of(side::try_lock(&file).map_err(Error::io("lock"))?),
                    // Finished meanwhile: renamed or removed by its writer.
                    Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(Error::io("open temporary file")(err)),
                },
            };
            files.push(SideFile {
                path,
                kind,
                in_use,
                removed: false,
            });
        }
        Ok(files)
    }

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
        self.remove_side_files(SideFileKind::Temporary)
    }

    /// Removes the quarantine copies that recoveries of this container made of corrupt tails (ADR-0014), and returns
    /// their paths. **A quarantine copy may hold acknowledged writes** that recovery could not prove stale: remove
    /// them only once an operator has inspected or saved them (`persia container sweep --quarantine`, ROADMAP 2.4.19).
    ///
    /// Needs a read-write handle opened by path: holding the writer lock, no recovery of this container can be adding
    /// one meanwhile.
    ///
    /// # Errors
    /// As [`Container::sweep_stale_files`].
    pub fn remove_quarantine_files(&self) -> Result<Vec<PathBuf>> {
        self.remove_side_files(SideFileKind::Quarantine)
    }

    /// Removes the side files of `kind` that no writer holds.
    fn remove_side_files(&self, kind: SideFileKind) -> Result<Vec<PathBuf>> {
        self.writable()?;
        let location = self.location.as_ref().ok_or_else(|| {
            Error::InvalidInput("the container was not opened from a path".into())
        })?;
        let vfs = &*location.vfs;
        let (dir, found) = scan(vfs, &location.path)?;
        let mut removed = Vec::new();
        for (path, _) in found.into_iter().filter(|(_, k)| *k == kind) {
            let file = match vfs.open(&path, false) {
                Ok(file) => file,
                // Gone meanwhile: its creator removed it.
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(Error::io("open side file")(err)),
            };
            // A temporary file held by a live create. On Windows files are not locked (the lock is beside the
            // container): no check, and the writer lock this handle holds excludes compactions.
            if side::try_lock(&file).map_err(Error::io("lock"))? == Some(false) {
                continue;
            }
            drop(file);
            match vfs.remove_file(&path) {
                Ok(()) => removed.push(path),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(Error::io("remove side file")(err)),
            }
        }
        if !removed.is_empty() {
            vfs.sync_dir(&dir).map_err(Error::io("sync directory"))?;
        }
        Ok(removed)
    }
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
