//! The file operations a container needs, so tests can run it on a simulated file that crashes ([`super::sim`]).

use std::fs::File;
use std::io;

use crate::local::fs;

/// Positioned reads and writes, `fdatasync` and truncation: all a [`Container`](super::Container) asks of its file.
///
/// Implemented for [`File`] and for [`SimFile`](super::sim::SimFile). Every method takes `&self`, like the
/// positioned I/O of [`File`]; the container serializes its writes.
pub trait ContainerFile: Send + Sync + std::fmt::Debug {
    /// The file's current length.
    ///
    /// # Errors
    /// The OS error.
    fn file_len(&self) -> io::Result<u64>;

    /// Fills `buf` with the bytes at `offset`.
    ///
    /// # Errors
    /// [`io::ErrorKind::UnexpectedEof`] if the file ends first, or the OS error.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;

    /// Writes all of `buf` at `offset`, growing the file if needed. Not durable until [`sync_data`](Self::sync_data).
    ///
    /// # Errors
    /// The OS error; part of `buf` may have been written.
    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()>;

    /// Makes every completed write and length change durable (`fdatasync`).
    ///
    /// # Errors
    /// The OS error. What reached the disk is then unknown, and repeating the call proves nothing: the OS may have
    /// dropped the unwritten data.
    fn sync_data(&self) -> io::Result<()>;

    /// Like [`sync_data`](Self::sync_data), and also makes the file's metadata (timestamps) durable (`fsync`, SPEC
    /// §4.5.2). The local backend uses it for new objects. Required, so each implementation states what it guarantees.
    ///
    /// # Errors
    /// As [`sync_data`](Self::sync_data).
    fn sync_all(&self) -> io::Result<()>;

    /// Truncates or extends the file to `len` bytes. Not durable until [`sync_data`](Self::sync_data).
    ///
    /// # Errors
    /// The OS error.
    fn set_len(&self, len: u64) -> io::Result<()>;

    /// Takes the exclusive writer lock without waiting: `Ok(false)` if another handle holds it, in this process or
    /// another. The lock lasts until the file is closed.
    ///
    /// # Errors
    /// The OS error.
    fn try_lock(&self) -> io::Result<bool>;

    /// Whether [`try_lock`](Self::try_lock) excludes other handles of this process too. Where a lock belongs to the
    /// process instead (`flock` emulated with POSIX locks: Linux NFS, some FUSE and CIFS mounts), a listing or sweep in
    /// the writer's process takes a lock the writer holds, and closing any descriptor of the file drops it, so a
    /// held lock proves nothing (ADR-0018). Defaults to `true`, as for simulated files.
    fn locks_per_handle(&self) -> bool {
        true
    }
}

impl ContainerFile for File {
    fn file_len(&self) -> io::Result<u64> {
        Ok(self.metadata()?.len())
    }

    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        fs::read_exact_at(self, buf, offset)
    }

    fn write_all_at(&self, buf: &[u8], offset: u64) -> io::Result<()> {
        fs::write_all_at(self, buf, offset)
    }

    fn sync_data(&self) -> io::Result<()> {
        File::sync_data(self)
    }

    fn sync_all(&self) -> io::Result<()> {
        File::sync_all(self)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }

    /// An advisory `flock` on Unix.
    #[cfg(not(windows))]
    fn try_lock(&self) -> io::Result<bool> {
        match File::try_lock(self) {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(err)) => Err(err),
        }
    }

    /// Only known on a local disk file system (ADR-0020): anywhere else, or if the file system cannot be queried,
    /// `false`, so a sweep does not trust the lock.
    #[cfg(not(windows))]
    fn locks_per_handle(&self) -> bool {
        crate::sys::is_local_disk(self).unwrap_or(false)
    }

    /// Unsupported on Windows: `LockFileEx` locks are mandatory, so a lock on the container would keep readers out.
    /// [`Container::create`](super::Container::create) and [`Container::open`](super::Container::open) lock a
    /// `<name>.lock` file next to it instead (SPEC §4.5.1); `create_on`, `open_on` (read-write) and `compact_into`
    /// with a bare [`File`] fail rather than run unlocked.
    #[cfg(windows)]
    fn try_lock(&self) -> io::Result<bool> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "on Windows a container's writer lock is the <name>.lock file beside it: use Container::create or open",
        ))
    }
}
