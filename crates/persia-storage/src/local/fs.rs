//! Platform file operations the standard library spells differently per OS (SPEC §4.5.2 documents them), shared
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

/// Windows: `FlushFileBuffers` on a directory handle, which needs `FILE_FLAG_BACKUP_SEMANTICS` to open a directory and
/// write access to flush it (SPEC §4.5.2, ROADMAP 2.4.8).
#[cfg(windows)]
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?
        .sync_all()
}

/// Neither Unix nor Windows: no directory sync is known.
#[cfg(not(any(unix, windows)))]
#[allow(clippy::unnecessary_wraps)] // same signature as the other versions
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

#[cfg(test)]
mod tests {
    use super::*;

    /// SPEC §4.5.2: a directory sync works on every platform CI runs (Linux, macOS, Windows), after creating,
    /// renaming and removing entries; a missing directory is an error, never a silent success.
    #[test]
    fn directories_can_be_synced() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        sync_dir(dir.path()).unwrap();
        std::fs::rename(dir.path().join("a"), dir.path().join("b")).unwrap();
        sync_dir(dir.path()).unwrap();
        std::fs::remove_file(dir.path().join("b")).unwrap();
        sync_dir(dir.path()).unwrap();
        assert!(sync_dir(&dir.path().join("missing")).is_err());
    }

    /// SPEC §4.5.2: `sync_data` and `sync_all` succeed on a regular file (`F_FULLFSYNC` on macOS, `FlushFileBuffers`
    /// on Windows, `fdatasync` / `fsync` elsewhere).
    #[test]
    fn files_can_be_synced() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(dir.path().join("f"))
            .unwrap();
        write_all_at(&file, b"data", 0).unwrap();
        file.sync_data().unwrap();
        file.sync_all().unwrap();
        let mut buf = [0; 4];
        read_exact_at(&file, &mut buf, 0).unwrap();
        assert_eq!(&buf, b"data");
    }
}
