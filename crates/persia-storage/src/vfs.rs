//! The file-system operations the local backend and the container's path API use, behind a trait, so crash tests can
//! run them on a simulated file system that loses unsynced data and directory entries (ROADMAP 2.4.11).
//!
//! [`RealFs`] is the operating system's. [`sim::SimFs`] keeps each directory's entries twice, as readers see them and
//! as a directory sync last made them durable, and its files are [`SimFile`](crate::container::sim::SimFile)s; a
//! crash keeps the durable state plus some, all or none of the rest. One fault schedule counts every mutating call
//! of the file system and its files, so a test can stop the "process" at any of them.

pub mod sim;

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::container::ContainerFile;
use crate::local::fs;

/// What a directory entry is, without following a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file of `len` bytes.
    File {
        /// Its length.
        len: u64,
    },
    /// A directory.
    Dir,
    /// Anything else: a symbolic link, a FIFO, a device. Persia never creates these.
    Other,
}

/// What a directory entry is, as the listing reports it (no `stat` where the file system records entry types), without
/// following a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// Anything else: a symbolic link, a FIFO, a device.
    Other,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's name.
    pub name: OsString,
    /// What it is. Its length takes a [`Vfs::entry_kind`].
    pub kind: FileType,
}

/// File-system operations, by path. Paths are absolute.
///
/// Every method that changes something counts as one mutating call for [`sim::SimFs`]'s fault schedule. Created,
/// renamed, linked and removed names are durable only after [`Vfs::sync_dir`] of their directory; file contents
/// only after [`ContainerFile::sync_data`].
pub trait Vfs: Send + Sync + std::fmt::Debug {
    /// An open file.
    type File: ContainerFile;

    /// Opens the existing regular file at `path`, read-only or read-write.
    ///
    /// # Errors
    /// [`io::ErrorKind::NotFound`] if it does not exist, or the OS error.
    fn open(&self, path: &Path, write: bool) -> io::Result<Self::File>;

    /// Creates the file at `path`, which must not exist, and opens it read-write.
    ///
    /// # Errors
    /// [`io::ErrorKind::AlreadyExists`] if `path` exists, [`io::ErrorKind::NotFound`] if its directory does not, or
    /// the OS error.
    fn create_new(&self, path: &Path) -> io::Result<Self::File>;

    /// Creates the directory `path`, whose parent must exist.
    ///
    /// # Errors
    /// [`io::ErrorKind::AlreadyExists`] if `path` exists, or the OS error.
    fn create_dir(&self, path: &Path) -> io::Result<()>;

    /// Renames `from` to `to`, replacing a file at `to` atomically.
    ///
    /// # Errors
    /// The OS error.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Gives the file at `from` the second name `to`.
    ///
    /// # Errors
    /// [`io::ErrorKind::AlreadyExists`] if `to` exists, or the OS error.
    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Removes the name `path` of a file.
    ///
    /// # Errors
    /// [`io::ErrorKind::NotFound`] if it does not exist, or the OS error.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    /// Makes the entries of directory `dir` durable.
    ///
    /// # Errors
    /// The OS error.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;

    /// What `path` is, without following a symbolic link at its end.
    ///
    /// # Errors
    /// [`io::ErrorKind::NotFound`] or [`io::ErrorKind::NotADirectory`] if it does not exist, or the OS error.
    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind>;

    /// Whether `path` is a directory, following symbolic links.
    ///
    /// # Errors
    /// [`io::ErrorKind::NotFound`] if it does not exist, or the OS error.
    fn is_dir(&self, path: &Path) -> io::Result<bool>;

    /// The entries of directory `dir`, in no particular order.
    ///
    /// # Errors
    /// [`io::ErrorKind::NotFound`] if `dir` does not exist, or the OS error.
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>>;

    /// The absolute path of `path` with symbolic links resolved.
    ///
    /// # Errors
    /// [`io::ErrorKind::NotFound`] if it does not exist, or the OS error.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;

    /// Whether `path` names the file open as `file`: same device and inode on Unix, same volume serial number and file
    /// id on Windows (ADR-0021).
    ///
    /// # Errors
    /// The OS error; a missing `path` is `Ok(false)`.
    fn same_file(&self, file: &Self::File, path: &Path) -> io::Result<bool>;

    /// Takes the container writer lock that does not live on the container file itself: on Windows a locked
    /// `<name>.lock` file next to `path` (SPEC §4.5.1), which the returned handle holds until dropped. `None` where
    /// the container file is locked instead ([`ContainerFile::try_lock`]).
    ///
    /// # Errors
    /// [`io::ErrorKind::WouldBlock`] if another handle holds it, or the OS error.
    fn lock_beside(&self, path: &Path) -> io::Result<Option<Self::File>>;
}

/// The operating system's file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealFs;

impl Vfs for RealFs {
    type File = File;

    fn open(&self, path: &Path, write: bool) -> io::Result<File> {
        let file = OpenOptions::new().read(true).write(write).open(path)?;
        // Unix opens a directory read-only without complaint; the contract is a regular file.
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a regular file", path.display()),
            ));
        }
        Ok(file)
    }

    fn create_new(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir(path)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn hard_link(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::hard_link(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        fs::sync_dir(dir)
    }

    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        let meta = std::fs::symlink_metadata(path)?;
        Ok(if meta.is_file() {
            EntryKind::File { len: meta.len() }
        } else if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::Other
        })
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        Ok(std::fs::metadata(path)?.is_dir())
    }

    fn read_dir(&self, dir: &Path) -> io::Result<Vec<DirEntry>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            // Does not follow symbolic links. Uses the listing's `d_type`, and `lstat` only on file systems without it;
            // an entry removed meanwhile is left out.
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            let kind = if file_type.is_dir() {
                FileType::Dir
            } else if file_type.is_file() {
                FileType::File
            } else {
                FileType::Other
            };
            out.push(DirEntry {
                name: entry.file_name(),
                kind,
            });
        }
        Ok(out)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }

    #[cfg(unix)]
    fn same_file(&self, file: &File, path: &Path) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let open = file.metadata()?;
        match std::fs::metadata(path) {
            Ok(named) => Ok((open.dev(), open.ino()) == (named.dev(), named.ino())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Volume serial number and file id (ROADMAP 2.4.18, ADR-0021): the standard library exposes neither on stable.
    #[cfg(windows)]
    fn same_file(&self, file: &File, path: &Path) -> io::Result<bool> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_READ_ATTRIBUTES: u32 = 0x80;
        // Attribute access only, which sharing modes do not restrict, as `std::fs::metadata` opens: no other handle, read
        // permission or directory can refuse it, and it is what the identity query needs.
        let named = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path);
        let named = match named {
            Ok(named) => named,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(err),
        };
        Ok(crate::sys::file_id(file)? == crate::sys::file_id(&named)?)
    }

    /// No file identity on other platforms: only whether `path` still names something is checked.
    #[cfg(not(any(unix, windows)))]
    fn same_file(&self, _file: &File, path: &Path) -> io::Result<bool> {
        match std::fs::metadata(path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    #[cfg(windows)]
    fn lock_beside(&self, path: &Path) -> io::Result<Option<File>> {
        let mut name = path
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
            .to_owned();
        name.push(".lock");
        // Created if missing and never removed: removing it would race with a writer taking it.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_file_name(name))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(std::fs::TryLockError::WouldBlock) => Err(io::ErrorKind::WouldBlock.into()),
            Err(std::fs::TryLockError::Error(err)) => Err(err),
        }
    }

    #[cfg(not(windows))]
    fn lock_beside(&self, _path: &Path) -> io::Result<Option<File>> {
        Ok(None)
    }
}

/// File identity on every platform with a real one: Unix (device, inode) and Windows (volume, file id; ROADMAP 2.4.18).
#[cfg(all(test, any(unix, windows)))]
mod identity_tests {
    use super::*;

    #[test]
    fn same_file_sees_a_rename_over_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"old").unwrap();
        let old = File::open(&path).unwrap();
        assert!(RealFs.same_file(&old, &path).unwrap());
        let other = dir.path().join("b");
        std::fs::write(&other, b"new").unwrap();
        let other_handle = File::open(&other).unwrap();
        assert!(!RealFs.same_file(&old, &other).unwrap(), "two files");
        assert!(
            RealFs.same_file(&other_handle, &other).unwrap(),
            "another handle, same file"
        );
        drop(other_handle);
        RealFs.rename(&other, &path).unwrap();
        assert!(
            !RealFs.same_file(&old, &path).unwrap(),
            "the path names another file now"
        );
        RealFs.remove_file(&path).unwrap();
        assert!(
            !RealFs.same_file(&old, &path).unwrap(),
            "the path names nothing"
        );
    }

    /// Windows: a directory, and a file another handle holds without sharing, still get an answer, not an error.
    #[cfg(windows)]
    #[test]
    fn same_file_answers_for_directories_and_unshared_files() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"x").unwrap();
        let file = File::open(&path).unwrap();
        assert!(!RealFs.same_file(&file, dir.path()).unwrap(), "a directory");
        drop(file);
        let exclusive = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert!(
            RealFs.same_file(&exclusive, &path).unwrap(),
            "held without sharing"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn entries_are_classified_without_following_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), b"abc").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("d"), dir.path().join("l")).unwrap();
        let mut entries = RealFs.read_dir(dir.path()).unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let kinds: Vec<_> = entries.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [FileType::Dir, FileType::File, FileType::Other]);
        assert_eq!(
            RealFs.entry_kind(&dir.path().join("f")).unwrap(),
            EntryKind::File { len: 3 }
        );
        assert_eq!(
            RealFs.entry_kind(&dir.path().join("l")).unwrap(),
            EntryKind::Other
        );
        assert!(
            RealFs.is_dir(&dir.path().join("l")).unwrap(),
            "is_dir follows links"
        );
        assert!(RealFs.lock_beside(dir.path()).unwrap().is_none());
    }
}
