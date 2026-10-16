//! The file operations a container needs, so tests can run it on a simulated file that crashes ([`super::sim`]).

use std::fs::File;
use std::io;

use crate::local::fs;

/// Positioned reads and writes, `fdatasync` and truncation: all a [`Container`](super::Container) asks of its file.
///
/// Implemented for [`File`] and for [`SimFile`](super::sim::SimFile). Every method takes `&self`, like the
/// positioned I/O of [`File`]; the container serializes its writes.
pub trait ContainerFile: Send + Sync {
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

    fn set_len(&self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }

    fn try_lock(&self) -> io::Result<bool> {
        // An advisory `flock` on Unix. On Windows `LockFileEx` locks are mandatory, so other handles cannot read the
        // file while a writer holds it (SPEC §4.5.1, ROADMAP 2.4.16).
        match File::try_lock(self) {
            Ok(()) => Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(err)) => Err(err),
        }
    }
}
