//! Platform file operations the standard library spells differently per OS (ROADMAP 2.4.8 documents them), shared
//! by the local backend and the container adapter.

use std::fs::File;
use std::io;
use std::path::Path;

/// Makes the entries of directory `dir` durable: a created, renamed or linked name survives a machine crash.
///
/// # Errors
/// The OS error from opening or syncing the directory.
#[cfg(unix)]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Windows cannot open a directory through the standard library; NTFS journals the metadata of a rename
/// (ROADMAP 2.4.8 documents and tests this).
#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // same signature as the Unix version
pub(crate) fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Fills `buf` from `file` at `offset` without moving a shared cursor (`pread`), so concurrent readers of one file
/// need no lock.
///
/// # Errors
/// [`io::ErrorKind::UnexpectedEof`] if the file ends first, or the OS error.
#[cfg(unix)]
pub(crate) fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Fills `buf` from `file` at `offset` (`ReadFile` with an offset); the file cursor moves, but every read here passes
/// its own offset.
///
/// # Errors
/// [`io::ErrorKind::UnexpectedEof`] if the file ends first, or the OS error.
#[cfg(windows)]
pub(crate) fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = buf.get_mut(n..).unwrap_or_default();
                offset = offset
                    .checked_add(u64::try_from(n).unwrap_or(u64::MAX))
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// Writes all of `buf` to `file` at `offset` (`pwrite`), without moving a shared cursor.
///
/// # Errors
/// The OS error; part of `buf` may have been written.
#[cfg(unix)]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Writes all of `buf` to `file` at `offset` (`WriteFile` with an offset); every write here passes its own offset.
///
/// # Errors
/// The OS error; part of `buf` may have been written.
#[cfg(windows)]
pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = buf.get(n..).unwrap_or_default();
                offset = offset
                    .checked_add(u64::try_from(n).unwrap_or(u64::MAX))
                    .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// The directory to sync for an entry of `path`: its parent, or the current directory for a relative single name.
pub(crate) fn parent_or_cwd(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}
