//! A consistent copy of a committed file and the durable, all-or-nothing writing of a backup (SPEC §4.8; ROADMAP
//! 10.1.8; ADR-0067).
//!
//! A [`CommittedImage`] is what one commit of a file of the §4.2 layout (the container, or the blob pack of SPEC §9.3)
//! needs: the 12288-byte prefix, its header and both slots holding that commit, built in memory when the image is
//! taken; and a handle on the file the commit was made in, to copy `12288..log_end` from. Below a committed `log_end`
//! a writer never writes again (SPEC §4.7), a compaction or a pack rewrite renames a new file over the path instead
//! (the handle keeps reading the old one), and the slots, the only bytes rewritten in place, are not read from the
//! file at all. So the copy is that commit's state whatever the writer does meanwhile, and it never reads what came
//! after: a WAL frame after `log_end`, a chunk staged for an upload not bound.
//!
//! [`write_backup_in`] writes images to new files that do not exist yet: each to a temporary name beside its
//! destination, synced; then each hard-linked to its destination (which fails if the name exists) and its directory
//! synced, in the order given, so a later file never survives a crash without an earlier one; then the temporary names
//! removed and the directory synced. A failure removes what it created.

use std::io;
use std::path::{Path, PathBuf};

use persia_format::superblock::FIRST_FRAME_OFFSET;
use persia_format::widen::u64_from_usize;

use super::path::temp_path;
use super::{ContainerFile, Error, Result};
use crate::local::fs::parent_or_cwd;
use crate::vfs::Vfs;

/// How much of a file a copy reads and writes at a time: bounded memory, whatever the size of a frame (as recovery
/// and compaction stream, SPEC §4.6).
const COPY_CHUNK: usize = 64 * 1024;

/// One committed state of a file of the §4.2 layout, copied on demand (see the module docs):
/// [`Container::committed_image`](super::Container::committed_image) for the container, `PackStore::committed_image`
/// (`persia-blob`) for the blob pack.
///
/// ```
/// use persia_format::block::Compression;
/// use persia_format::frame::FrameKind;
/// use persia_storage::container::{Commit, Container, ContainerFile, Mode};
///
/// # let dir = tempfile::tempdir()?;
/// let path = dir.path().join("app.persia");
/// let mut c = Container::create(&path)?;
/// let manifest = c.append(FrameKind::Manifest, b"manifest 1", Compression::None)?;
/// c.commit(Commit { manifest, wal_start: manifest.offset, fence_token: 0 })?;
/// let image = c.committed_image()?;
/// // Written after the image was taken: not in the copy.
/// c.append(FrameKind::WalChunk, b"later", Compression::None)?;
/// c.sync()?;
///
/// let copy = std::fs::File::create_new(dir.path().join("copy.persia"))?;
/// image.write_to(&copy)?;
/// assert_eq!(copy.file_len()?, image.len());
/// let opened = Container::open(dir.path().join("copy.persia"), Mode::ReadOnly)?;
/// assert_eq!(opened.superblock(), c.superblock());
/// assert!(opened.uncommitted_frames().is_empty());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct CommittedImage<F> {
    /// The header page and both slots, each holding the commit.
    prefix: Vec<u8>,
    /// A handle on the file the commit was made in.
    file: F,
    /// The commit's `log_end`: where the copy ends.
    end: u64,
}

impl<F: ContainerFile> CommittedImage<F> {
    /// The image of the commit whose prefix (header page and both slots, [`FIRST_FRAME_OFFSET`] bytes) is `prefix`,
    /// whose frames end at `end`, made in `file`.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `prefix` is not [`FIRST_FRAME_OFFSET`] bytes or `end` is below it;
    /// [`Error::Format`] (corruption) if `file` is shorter than `end`: a committed frame is missing;
    /// [`Error::Io`] if its length cannot be read.
    pub fn new(prefix: Vec<u8>, file: F, end: u64) -> Result<Self> {
        if u64_from_usize(prefix.len()) != FIRST_FRAME_OFFSET || end < FIRST_FRAME_OFFSET {
            return Err(Error::InvalidInput(format!(
                "an image needs a prefix of {FIRST_FRAME_OFFSET} bytes and an end at or past it, not {} and {end}",
                prefix.len()
            )));
        }
        let file_len = file.file_len().map_err(Error::io("stat"))?;
        if file_len < end {
            return Err(Error::Format(persia_format::Error::Corrupt {
                offset: file_len,
                reason: persia_format::Corruption::UnexpectedEof {
                    needed: end,
                    available: file_len,
                },
            }));
        }
        Ok(Self { prefix, file, end })
    }

    /// The length of the copy: the commit's `log_end`.
    pub fn len(&self) -> u64 {
        self.end
    }

    /// Whether the copy is only the prefix: a file that was never committed past it.
    pub fn is_empty(&self) -> bool {
        self.end == FIRST_FRAME_OFFSET
    }

    /// Writes the copy into `target`, an empty file, and syncs it (data and length).
    ///
    /// # Errors
    /// [`Error::InvalidInput`] if `target` is not empty; [`Error::Io`] if a read, a write or the sync fails (what
    /// `target` then holds is garbage).
    pub fn write_to<G: ContainerFile>(&self, target: &G) -> Result<()> {
        let target_len = target.file_len().map_err(Error::io("stat"))?;
        if target_len != 0 {
            return Err(Error::InvalidInput(format!(
                "an image is written into an empty file, not one of {target_len} bytes"
            )));
        }
        target
            .write_all_at(&self.prefix, 0)
            .map_err(Error::io("write the copy"))?;
        let mut buf = vec![0; COPY_CHUNK];
        let mut at = FIRST_FRAME_OFFSET;
        while at < self.end {
            let n = usize::try_from(self.end.saturating_sub(at))
                .map_or(COPY_CHUNK, |left| left.min(COPY_CHUNK));
            let chunk = buf.get_mut(..n).ok_or_else(|| {
                Error::InvalidInput("a copy chunk larger than its buffer".to_owned())
            })?;
            self.file
                .read_exact_at(chunk, at)
                .map_err(Error::io("read the original"))?;
            target
                .write_all_at(chunk, at)
                .map_err(Error::io("write the copy"))?;
            at = at.saturating_add(u64_from_usize(n));
        }
        target.sync_all().map_err(Error::io("sync the copy"))
    }
}

/// Writes each image of `files` to its path on `vfs`, as a new file, durably and all or nothing (see the module docs):
/// nothing is written unless every path is free; then the files are written under temporary names and synced, and
/// linked to their paths in the order given, each link's directory synced before the next link. Either every path
/// names its complete copy, or (after a failure this returns) none of them names anything this call made, unless
/// removing a link it made fails too, which is logged. A crash
/// leaves a prefix of `files` linked, each complete, and maybe stale temporary files `<name>.backup.<nonce>.<n>`.
/// The file system must support hard links.
///
/// ```
/// use persia_storage::container::{Container, write_backup_in};
/// use persia_storage::vfs::RealFs;
///
/// # let dir = tempfile::tempdir()?;
/// let c = Container::create(dir.path().join("app.persia"))?;
/// let dest = dir.path().join("backup.persia");
/// write_backup_in(&RealFs, &[(dest.as_path(), &c.committed_image()?)])?;
/// assert!(dest.is_file());
/// // A path that exists is never replaced.
/// assert!(write_backup_in(&RealFs, &[(dest.as_path(), &c.committed_image()?)]).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
/// [`Error::AlreadyExists`] if a path exists (before anything is written) or appears before its link;
/// [`Error::InvalidInput`] for a path with no file name; [`Error::Io`] if the file system fails. An `Io` error from
/// removing a temporary name or the last directory sync comes after every link: the copies are then complete at their
/// paths, but the last sync may not have made the removal durable, and a retry gets `AlreadyExists`.
pub fn write_backup_in<V: Vfs + ?Sized, F: ContainerFile>(
    vfs: &V,
    files: &[(&Path, &CommittedImage<F>)],
) -> Result<()> {
    for (path, _) in files {
        match vfs.entry_kind(path) {
            Err(e) if matches!(e.kind(), io::ErrorKind::NotFound) => {}
            Err(e) => return Err(Error::io("look for the backup's destination")(e)),
            Ok(_) => {
                return Err(Error::AlreadyExists {
                    path: (*path).to_owned(),
                });
            }
        }
    }
    let mut temps: Vec<PathBuf> = Vec::with_capacity(files.len());
    let written = files.iter().try_for_each(|(path, image)| {
        let temp = temp_path(path, ".backup")?;
        let file = vfs.create_new(&temp).map_err(Error::io("create"))?;
        temps.push(temp);
        image.write_to(&file)
    });
    let mut linked: Vec<&Path> = Vec::with_capacity(files.len());
    let result = written.and_then(|()| {
        files.iter().zip(&temps).try_for_each(|((path, _), temp)| {
            vfs.hard_link(temp, path).map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    Error::AlreadyExists {
                        path: (*path).to_owned(),
                    }
                } else {
                    Error::io("link")(e)
                }
            })?;
            linked.push(path);
            // Before the next link: a later file never survives a crash without this one.
            vfs.sync_dir(parent_or_cwd(path))
                .map_err(Error::io("sync directory"))
        })
    });
    if let Err(e) = result {
        undo(vfs, &linked, &temps);
        return Err(e);
    }
    let mut removed = Ok(());
    for temp in &temps {
        if let Err(e) = vfs.remove_file(temp)
            && e.kind() != io::ErrorKind::NotFound
            && removed.is_ok()
        {
            removed = Err(Error::io("remove temporary file")(e));
        }
    }
    let mut synced = Ok(());
    for dir in directories(temps.iter().map(PathBuf::as_path)) {
        if let Err(e) = vfs.sync_dir(dir)
            && synced.is_ok()
        {
            synced = Err(Error::io("sync directory")(e));
        }
    }
    removed.and(synced)
}

/// After a failure: removes the paths linked so far (newest first, so no later file outlives an earlier one) and the
/// temporary names, then syncs their directories. Its own failures are logged: the error the caller gets is the
/// first one, and what is left is a prefix of the files (complete) or stale temporary names.
fn undo<V: Vfs + ?Sized>(vfs: &V, linked: &[&Path], temps: &[PathBuf]) {
    for path in linked.iter().rev() {
        if let Err(e) = vfs.remove_file(path) {
            tracing::error!(path = %path.display(), error = %e, "removing a partial backup failed");
        }
    }
    for temp in temps {
        if let Err(e) = vfs.remove_file(temp)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(path = %temp.display(), error = %e, "removing a backup's temporary file failed");
        }
    }
    for dir in directories(
        linked
            .iter()
            .copied()
            .chain(temps.iter().map(PathBuf::as_path)),
    ) {
        if let Err(e) = vfs.sync_dir(dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "syncing a directory after a failed backup failed");
        }
    }
}

/// The distinct parent directories of `paths`, in first-seen order.
fn directories<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<&'a Path> {
    let mut dirs: Vec<&Path> = Vec::new();
    for path in paths {
        let dir = parent_or_cwd(path);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}
